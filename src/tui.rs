//! Transcript TUI replicating the vercel-labs/fx terminal UX.
//!
//! Inline mode (default) streams the transcript into the terminal scrollback
//! with the footer pinned at the bottom, so the terminal's native scrolling
//! (mouse wheel, Shift+PgUp) works on sessions. Ctrl+O opens the
//! full-transcript mode with internal PgUp/PgDn/wheel scrolling.

use crate::markdown::{self, Block, Markdown};
use crate::openai::OpenAI;
use crate::run::{self, Outcome, RunOptions, Sink};
use crate::session::{self, SessionMeta};
use crate::term::{self, Key, Terminal};
use crate::{Message, Tool, ToolCall, Usage};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[38;5;245m";
const DIVIDER: &str = "\x1b[38;5;240m";
const ACTIVITY: &str = "\x1b[38;5;252m";
const HINT: &str = "\x1b[38;5;255m";
const SELECTED: &str = "\x1b[1;38;5;255m";
const USER_RAIL: &str = "\x1b[38;5;255m";
const WELCOME_APP: &str = "\x1b[1;38;5;255m";

pub struct TuiConfig {
    pub base: String,
    pub model: String,
    pub system: String,
    pub dir: String,
    pub axe_root: String,
    pub session_dir: String,
    pub api_key: String,
    /// None = fresh session. Some("") = resume picker. Some("last") or id = load.
    pub resume: Option<String>,
    /// Model context window in tokens; unset means the model's default applies.
    pub context_window: Option<usize>,
}

enum Entry {
    Welcome,
    User(String),
    Text(String),
    Code(String),
    Table(String),
    Rule,
    Tool {
        calls: Vec<String>,
    },
    Notice(String),
    Summary {
        secs: u64,
        input: usize,
        output: usize,
    },
}

#[derive(PartialEq, Clone)]
enum Activity {
    Idle,
    Thinking,
    Streaming,
}

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    None,
    Help,
    Resume,
    Rewind,
}

#[derive(Clone)]
struct RewindItem {
    msg_idx: usize,
    role: String,
    preview: String,
}

/// Two Esc presses within this window open the rewind screen.
const REWIND_ESC_MS: u128 = 800;

pub enum TurnEvent {
    AssistantDelta(String),
    AssistantDone,
    ToolStart {
        label: String,
        kind: String,
    },
    ToolDelta(String),
    ToolResult {
        label: String,
        kind: String,
    },
    Tokens {
        input: usize,
        output: usize,
        cached_input: usize,
    },
    Notice(String),
    End {
        messages: Vec<Message>,
        usage: Usage,
        err: Option<String>,
        cancelled: bool,
        compact: bool,
    },
}

struct SlashSpec {
    command: &'static str,
    help: &'static str,
    description: &'static str,
    category: &'static str,
}

struct SlashItem {
    command: String,
    help: String,
    description: String,
    category: String,
}

impl SlashItem {
    fn builtin(spec: &SlashSpec) -> SlashItem {
        SlashItem {
            command: spec.command.to_string(),
            help: spec.help.to_string(),
            description: spec.description.to_string(),
            category: spec.category.to_string(),
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum PickerKind {
    Slash,
    Files,
}

struct Picker {
    kind: PickerKind,
    token_start: usize,
    token_end: usize,
    query: String,
    sel: usize,
    win: usize,
    slash_matches: Vec<SlashItem>,
    file_matches: Vec<String>,
}

const PICKER_VISIBLE: usize = 6;

const SLASH: &[SlashSpec] = &[
    SlashSpec {
        command: "/help",
        help: "/help",
        description: "show available slash commands",
        category: "General",
    },
    SlashSpec {
        command: "/new",
        help: "/new",
        description: "start a fresh session",
        category: "Session",
    },
    SlashSpec {
        command: "/reset",
        help: "/reset",
        description: "reset the current session context",
        category: "Session",
    },
    SlashSpec {
        command: "/resume",
        help: "/resume",
        description: "resume a saved session",
        category: "Session",
    },
    SlashSpec {
        command: "/rewind",
        help: "/rewind",
        description: "rewind the session to an earlier message",
        category: "Session",
    },
    SlashSpec {
        command: "/compact",
        help: "/compact",
        description: "summarize the conversation so far",
        category: "Session",
    },
    SlashSpec {
        command: "/copy",
        help: "/copy",
        description: "copy the last assistant response",
        category: "Session",
    },
    SlashSpec {
        command: "/quit",
        help: "/quit",
        description: "exit the interactive shell",
        category: "General",
    },
];

pub fn run(cfg: TuiConfig) -> Result<(), String> {
    let mut term = Terminal::new()?;
    let mut tui = Tui::new(cfg);
    if let Some(id) = tui.cfg.resume.clone() {
        if id.is_empty() {
            tui.open_screen(Screen::Resume);
        } else {
            tui.resume_by_id(&id);
        }
    } else {
        session::archive_live(&tui.cfg.session_dir);
    }
    if tui.entries.is_empty() {
        tui.entries.push(Entry::Welcome);
    }
    tui.paint(&mut term);
    let result = tui.loop_forever(&mut term);
    term.restore();
    let mut out = std::io::stdout();
    let _ = write!(out, "{}", term::move_to(tui.last_input_row, 1));
    let _ = out.write_all(b"\x1b[J\n");
    let _ = out.flush();
    if result.is_ok() {
        tui.on_exit();
    }
    result
}

type PendingBlocks = Rc<RefCell<Vec<(String, Block)>>>;

#[allow(clippy::type_complexity)]
type CompactResult = Result<(String, usize, Vec<Message>), String>;

struct Tui {
    cfg: TuiConfig,
    entries: Vec<Entry>,
    running: bool,
    cancel: Arc<AtomicBool>,
    ctrl_c_pending: bool,
    ctrl_c_armed_ms: Option<Instant>,
    esc_armed_ms: Option<Instant>,
    last_input_row: u16,
    exit_alt_pending: bool,
    tx: Option<Sender<TurnEvent>>,
    rx: Option<Receiver<TurnEvent>>,
    steer_tx: Option<Sender<String>>,
    compacting: bool,
    compact_rx: Option<Receiver<CompactResult>>,
    retry_after_compact: bool,
    overflow_retried: bool,
    cur_text: Option<usize>,
    md: Option<Markdown>,
    md_pending: Option<PendingBlocks>,
    msgs: Vec<Message>,
    activity: Activity,
    tool_running: Option<String>,
    tool_live: Option<String>,
    pending_tools: Vec<String>,
    turn_start: Instant,
    input: Input,
    model_display: String,
    sess_in: usize,
    sess_out: usize,
    want_quit: bool,
    screen: Screen,
    alt_active: bool,
    streamed: Vec<String>,
    painted_once: bool,
    last_capacity: usize,
    last_frame: Vec<String>,
    last_chrome: Option<(usize, Vec<String>, usize, usize)>,
    reprint: bool,
    live_in: usize,
    live_out: usize,
    live_cached_in: usize,
    rows: u16,
    cols: u16,
    sel: usize,
    window_start: usize,
    sessions: Vec<SessionMeta>,
    picker: Option<Picker>,
    picker_dismissed: Option<PickerKind>,
    rewind_items: Vec<RewindItem>,
    /// Archive id being continued; None = a fresh session that forks on exit.
    resume_id: Option<String>,
}

impl Tui {
    fn new(cfg: TuiConfig) -> Tui {
        let model_display = compact_model_label(&cfg.model);
        Tui {
            cfg,
            entries: Vec::new(),
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            ctrl_c_pending: false,
            ctrl_c_armed_ms: None,
            esc_armed_ms: None,
            last_input_row: 1,
            exit_alt_pending: false,
            tx: None,
            rx: None,
            steer_tx: None,
            compacting: false,
            compact_rx: None,
            retry_after_compact: false,
            overflow_retried: false,
            cur_text: None,
            md: None,
            md_pending: None,
            msgs: Vec::new(),
            activity: Activity::Idle,
            tool_running: None,
            tool_live: None,
            pending_tools: Vec::new(),
            turn_start: Instant::now(),
            input: Input::default(),
            model_display,
            sess_in: 0,
            sess_out: 0,
            want_quit: false,
            screen: Screen::None,
            alt_active: false,
            streamed: Vec::new(),
            painted_once: false,
            last_capacity: 0,
            last_frame: Vec::new(),
            last_chrome: None,
            reprint: false,
            live_in: 0,
            live_out: 0,
            live_cached_in: 0,
            rows: 24,
            cols: 80,
            sel: 0,
            window_start: 0,
            sessions: Vec::new(),
            picker: None,
            picker_dismissed: None,
            rewind_items: Vec::new(),
            resume_id: None,
        }
    }

    fn on_exit(&mut self) {
        let entries = session::load_live(&self.cfg.session_dir);
        let projected = session::context_messages(&entries).len();
        let mut entries = entries;
        for m in &self.msgs[projected.min(self.msgs.len())..] {
            entries.push(session::Entry::Message { message: m.clone() });
        }
        match self.resume_id.take() {
            Some(id) => {
                session::continue_archived(&self.cfg.session_dir, &id, &entries);
            }
            None => {
                session::save_live(&self.cfg.session_dir, &entries);
                session::archive_live(&self.cfg.session_dir);
            }
        }
    }

    fn loop_forever(&mut self, term: &mut Terminal) -> Result<(), String> {
        let stdin_fd = libc::STDIN_FILENO;
        loop {
            let mut fds = [libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            }];
            unsafe {
                libc::poll(fds.as_mut_ptr(), 1, 40);
            }
            if fds[0].revents & libc::POLLIN != 0 && !self.handle_key(term.read_key()?)? {
                break;
            }
            if self.want_quit {
                break;
            }
            if self.exit_alt_pending {
                self.exit_alt_pending = false;
                self.leave_alt(term);
            }
            if let Some(t) = self.ctrl_c_armed_ms
                && t.elapsed().as_millis() >= 3000
            {
                self.ctrl_c_armed_ms = None;
                self.ctrl_c_pending = false;
            }
            if let Some(t) = self.esc_armed_ms
                && t.elapsed().as_millis() >= REWIND_ESC_MS
            {
                self.esc_armed_ms = None;
            }
            self.drain_events();
            self.paint(term);
        }
        Ok(())
    }

    fn ctrl_letter(c: char) -> Option<char> {
        let b = c as u8;
        if (1..=26).contains(&b) {
            Some((b + 96) as char)
        } else {
            None
        }
    }

    fn handle_key(&mut self, key: Key) -> Result<bool, String> {
        match key {
            Key::CtrlC | Key::Esc => {}
            Key::Ctrl(c) if c as u8 == 3 => {}
            _ => {
                self.ctrl_c_pending = false;
                self.ctrl_c_armed_ms = None;
                self.esc_armed_ms = None;
            }
        }
        if self.screen != Screen::None {
            return self.handle_screen_key(key);
        }
        match key {
            Key::CtrlC => {
                self.ctrl_c();
                return Ok(true);
            }
            Key::Ctrl(c) if c as u8 == 3 => {
                // Ctrl+C via modifyOtherKeys (ESC[27;5;99~)
                self.ctrl_c();
                return Ok(true);
            }
            Key::Ctrl(c) => {
                let letter = Self::ctrl_letter(c).unwrap_or(c);
                if !self.handle_ctrl(letter) {
                    return Ok(false);
                }
            }
            Key::Char(c) => self.input.insert(c),
            Key::Enter => {
                if self.picker.is_some() {
                    self.picker_enter();
                } else if self.running {
                    self.steer();
                } else {
                    self.submit();
                    if self.want_quit {
                        return Ok(false);
                    }
                }
            }
            Key::ShiftEnter => self.input.insert('\n'),
            Key::Tab => {
                if self.picker.is_some() {
                    self.picker_tab();
                }
            }
            Key::ShiftTab => {
                if self.picker.is_some() {
                    self.picker_move(true);
                }
            }
            Key::Backspace => self.input.backspace(),
            Key::Delete => self.input.delete(),
            Key::Left => self.input.move_left(),
            Key::Right => self.input.move_right(),
            Key::Home => self.input.home(),
            Key::End => self.input.end(),
            Key::CtrlHome => self.input.doc_home(),
            Key::CtrlEnd => self.input.doc_end(),
            Key::Up => {
                if self.picker.is_some() {
                    self.picker_move(true);
                } else {
                    let (line, _) = self.input.cursor_line_col();
                    if line > 0 {
                        self.input.move_line_up();
                    } else {
                        self.input.history_prev();
                    }
                }
            }
            Key::Down => {
                if self.picker.is_some() {
                    self.picker_move(false);
                } else {
                    let lines = self.input.buf().chars().filter(|c| *c == '\n').count();
                    let (line, _) = self.input.cursor_line_col();
                    if line < lines {
                        self.input.move_line_down();
                    } else {
                        self.input.history_next();
                    }
                }
            }
            Key::AltLeft | Key::CtrlLeft => self.input.move_word_left(),
            Key::AltRight | Key::CtrlRight => self.input.move_word_right(),
            Key::AltUp | Key::AltDown | Key::CtrlUp | Key::CtrlDown => {}
            Key::PageUp | Key::PageDown => {}
            Key::Alt(c) if c == '\r' || c == '\n' => self.input.insert('\n'),
            Key::Alt(_) => self.input.esc(),
            Key::Esc => {
                if self.picker.is_some() {
                    self.picker_dismiss();
                } else {
                    self.input.esc();
                    if self.double_esc() {
                        if self.busy_for_rewind() {
                            self.entries.push(Entry::Notice(
                                "agent is running; ctrl+c interrupts it first".into(),
                            ));
                        } else {
                            self.open_screen(Screen::Rewind);
                        }
                    } else {
                        self.esc_armed_ms = Some(Instant::now());
                    }
                }
            }
            Key::Paste(bytes) => self.input.paste(&bytes),
            Key::PasteEnd => {}
            Key::Eof => return Ok(false),
        }
        Ok(true)
    }

    fn handle_screen_key(&mut self, key: Key) -> Result<bool, String> {
        match key {
            Key::CtrlC => {
                let now = Instant::now();
                let within = self
                    .ctrl_c_armed_ms
                    .map(|t| now.duration_since(t).as_millis() < 3000)
                    .unwrap_or(false);
                if within {
                    self.want_quit = true;
                } else {
                    self.ctrl_c_armed_ms = Some(now);
                    self.ctrl_c_pending = true;
                    self.close_screen();
                }
                Ok(true)
            }
            Key::Ctrl(c) => {
                let letter = Self::ctrl_letter(c).unwrap_or(c);
                if letter == 'o' || letter == 'l' {
                    self.close_screen();
                }
                Ok(true)
            }
            Key::Esc => {
                self.close_screen();
                Ok(true)
            }
            Key::Char(c) => {
                self.input.insert(c);
                self.sel = 0;
                self.window_start = 0;
                Ok(true)
            }
            Key::Backspace => {
                self.input.backspace();
                self.sel = 0;
                self.window_start = 0;
                Ok(true)
            }
            Key::Up => {
                self.sel = self.sel.saturating_sub(1);
                Ok(true)
            }
            Key::Down => {
                let n = self.catalog_item_count();
                if n > 0 && self.sel + 1 < n {
                    self.sel += 1;
                }
                Ok(true)
            }
            Key::PageUp => {
                self.sel = self.sel.saturating_sub(8);
                Ok(true)
            }
            Key::PageDown => {
                let n = self.catalog_item_count();
                self.sel = (self.sel + 8).min(n.saturating_sub(1));
                Ok(true)
            }
            Key::Left | Key::Right | Key::Tab => Ok(true),
            Key::Enter => {
                self.catalog_activate();
                Ok(true)
            }
            Key::Paste(bytes) => {
                for c in String::from_utf8_lossy(&bytes).chars() {
                    if c == '\n' || c == '\r' {
                        continue;
                    }
                    self.input.insert(c);
                }
                self.sel = 0;
                self.window_start = 0;
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn close_screen(&mut self) {
        self.screen = Screen::None;
        self.input.take();
        self.exit_alt_pending = true;
        self.streamed.clear();
    }

    fn enter_alt(&mut self, term: &mut Terminal) {
        if self.alt_active {
            return;
        }
        let out = term.out();
        let _ = out.write_all(term::enter_alt().as_bytes());
        let _ = out.write_all(term::clear_display().as_bytes());
        let _ = out.flush();
        self.alt_active = true;
    }

    fn leave_alt(&mut self, term: &mut Terminal) {
        if !self.alt_active {
            return;
        }
        let out = term.out();
        let _ = out.write_all(term::leave_alt().as_bytes());
        let _ = out.flush();
        self.alt_active = false;
    }

    /// Consume an Esc press: true when it completes a double-Esc within
    /// REWIND_ESC_MS of the previous one.
    fn double_esc(&mut self) -> bool {
        let now = Instant::now();
        let within = self
            .esc_armed_ms
            .map(|t| now.duration_since(t).as_millis() < REWIND_ESC_MS)
            .unwrap_or(false);
        self.esc_armed_ms = None;
        within
    }

    fn busy_for_rewind(&self) -> bool {
        self.running || self.compacting
    }

    fn ctrl_c(&mut self) {
        let now = Instant::now();
        let within = self
            .ctrl_c_armed_ms
            .map(|t| now.duration_since(t).as_millis() < 3000)
            .unwrap_or(false);
        if within {
            if self.running {
                self.cancel.store(true, Ordering::Relaxed);
            }
            self.want_quit = true;
        } else {
            self.ctrl_c_armed_ms = Some(now);
            self.ctrl_c_pending = true;
            if self.running {
                self.cancel.store(true, Ordering::Relaxed);
            }
            if !self.input.buf.is_empty() {
                self.input.buf.clear();
                self.input.cursor = 0;
            }
        }
    }

    fn handle_ctrl(&mut self, c: char) -> bool {
        match c {
            'a' => self.input.home(),
            'e' => self.input.end(),
            'b' => self.input.move_left(),
            'f' => self.input.move_right(),
            'p' => self.input.history_prev(),
            'n' => self.input.history_next(),
            'w' => self.input.delete_word_left(),
            'u' => {
                let chars: Vec<char> = self.input.buf.chars().collect();
                self.input.buf = chars[self.input.cursor..].iter().collect();
                self.input.cursor = 0;
            }
            'k' => {
                let chars: Vec<char> = self.input.buf.chars().collect();
                self.input.buf = chars[..self.input.cursor].iter().collect();
            }
            'd' => {
                if !self.running && self.input.buf.is_empty() {
                    return false;
                }
                if self.input.cursor < self.input.buf.chars().count() {
                    self.input.delete();
                }
            }
            'l' => {
                self.painted_once = false;
            }
            _ => {}
        }
        true
    }

    fn steer(&mut self) {
        let v = self.input.take();
        if v.trim().is_empty() {
            return;
        }
        match &self.steer_tx {
            Some(tx) if tx.send(v.clone()).is_ok() => {
                self.entries.push(Entry::User(v));
            }
            _ => {
                // The run just ended (worker dropped the steer receiver). Keep
                // the draft in the input instead of stranding it in the
                // transcript; the next Enter submits normally.
                self.input.buf = v;
                self.entries
                    .push(Entry::Notice("agent finished; press enter to send".into()));
            }
        }
    }

    fn submit(&mut self) {
        if self.compacting {
            self.entries.push(Entry::Notice("compacting…".into()));
            return;
        }
        let v = self.input.take();
        if v.is_empty() {
            return;
        }
        if let Some(rest) = v.strip_prefix('/') {
            self.slash(rest);
            return;
        }
        if self.running {
            return;
        }
        self.entries.push(Entry::User(v.clone()));
        self.input.history.push(v.clone());
        self.msgs.push(Message {
            role: "user".into(),
            content: v,
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
        });
        self.start_turn();
    }

    /// Append the new messages from a finished run to the session entries,
    /// save, and schedule compaction when the context is over budget or the
    /// run failed with a context overflow error (retrying once after).
    fn persist_session(
        &mut self,
        messages: &[Message],
        usage: Usage,
        err: Option<&str>,
        resume_after_compaction: bool,
    ) {
        let mut entries = session::load_live(&self.cfg.session_dir);
        let projected_len = session::context_messages(&entries).len();
        let new_msgs: Vec<Message> = if messages.len() > projected_len {
            messages[projected_len..].to_vec()
        } else {
            Vec::new()
        };
        for m in &new_msgs {
            entries.push(session::Entry::Message { message: m.clone() });
        }
        if usage.input > 0 || usage.output > 0 {
            entries.push(session::Entry::Usage {
                input: usage.input,
                output: usage.output,
                cached_input: self.live_cached_in,
                context_input: self.live_in,
                context_output: self.live_out,
            });
        }
        session::save_live(&self.cfg.session_dir, &entries);

        if resume_after_compaction && !self.compacting {
            self.retry_after_compact = true;
            self.start_compaction(entries);
            return;
        }
        let overflow = err.map(session::is_overflow_error).unwrap_or(false);
        if overflow && !self.overflow_retried && !self.compacting {
            self.overflow_retried = true;
            self.retry_after_compact = true;
            self.start_compaction(entries);
            return;
        }
        if overflow || self.compacting {
            return;
        }
        let threshold = self
            .cfg
            .context_window
            .map(|window| window.saturating_sub(16384));
        if let Some(threshold) = threshold
            && session::latest_context_tokens(&entries).is_some_and(|tokens| tokens > threshold)
        {
            self.start_compaction(entries);
        }
    }

    fn start_compaction(&mut self, entries: Vec<session::Entry>) {
        self.compacting = true;
        self.entries.push(Entry::Notice("compacting…".into()));
        let provider = OpenAI::new(self.cfg.base.clone(), self.cfg.api_key.clone());
        let model = self.cfg.model.clone();
        let (ctx_tx, ctx_rx) = std::sync::mpsc::channel();
        self.compact_rx = Some(ctx_rx);
        std::thread::spawn(move || {
            let result = session::compact(&provider, &model, &entries);
            let _ = ctx_tx.send(result);
        });
    }

    fn finish_compaction(&mut self, summary: String, tokens_before: usize, retained: Vec<Message>) {
        self.compacting = false;
        let entry = session::Entry::Compaction {
            summary,
            tokens_before,
            timestamp: session::now_ms(),
            retained,
        };
        // Append-only: the summary entry joins the existing entries; the
        // context projection drops what it supersedes.
        let mut entries = session::load_live(&self.cfg.session_dir);
        entries.push(entry);
        session::save_live(&self.cfg.session_dir, &entries);
        self.msgs = session::context_messages(&entries);
        self.live_in = 0;
        self.live_out = 0;
        self.live_cached_in = 0;
        self.entries.push(Entry::Notice("compacted".into()));
        if self.retry_after_compact {
            self.retry_after_compact = false;
            self.start_turn();
        }
    }

    fn start_turn(&mut self) {
        if self.compacting {
            return;
        }
        session::trim_trailing_tool_messages(&mut self.msgs);
        let msgs = self.msgs.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx = Some(tx.clone());
        self.rx = Some(rx);
        let (steer_tx, steer_rx) = std::sync::mpsc::channel();
        self.steer_tx = Some(steer_tx);
        self.running = true;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.ctrl_c_pending = false;
        self.ctrl_c_armed_ms = None;
        self.turn_start = Instant::now();
        self.activity = Activity::Thinking;
        self.cur_text = None;
        self.live_in = 0;
        self.live_out = 0;
        self.live_cached_in = 0;
        let cancel = self.cancel.clone();
        let provider = OpenAI::new(self.cfg.base.clone(), self.cfg.api_key.clone());
        let model = self.cfg.model.clone();
        let system = self.cfg.system.clone();
        let dir = self.cfg.dir.clone();
        let tools = build_tools(&dir);
        let threshold = self
            .cfg
            .context_window
            .map(|window| window.saturating_sub(16384));
        std::thread::spawn(move || {
            let end = {
                let mut sink = TuiSink {
                    tx: &tx,
                    steer: steer_rx,
                    threshold,
                };
                run::run_stream(
                    &provider,
                    &RunOptions {
                        model: &model,
                        system: &system,
                        tools: &tools,
                        max_turns: usize::MAX,
                    },
                    &msgs,
                    &cancel,
                    &mut sink,
                )
            };
            let (err, cancelled, compact) = match end.outcome {
                Outcome::Done => (None, false, false),
                Outcome::MaxTurns => (Some("stopped: max turns reached".into()), false, false),
                Outcome::Cancelled => (None, true, false),
                Outcome::Compact => (None, false, true),
                Outcome::Failed(e) => (Some(e), false, false),
            };
            let _ = tx.send(TurnEvent::End {
                messages: end.messages,
                usage: end.usage,
                err,
                cancelled,
                compact,
            });
        });
    }

    fn drain_events(&mut self) -> bool {
        let mut any = false;
        let mut cur = self.cur_text;
        if let Some(rx) = self.rx.take() {
            while let Ok(ev) = rx.try_recv() {
                any = true;
                match ev {
                    TurnEvent::AssistantDelta(delta) => {
                        self.flush_tools();
                        if self.md.is_none() {
                            let pending = Rc::new(RefCell::new(Vec::new()));
                            let p2 = pending.clone();
                            let mut md = Markdown::new();
                            md.set_on_block(move |b, out| {
                                p2.borrow_mut().push((std::mem::take(out), b));
                            });
                            self.md = Some(md);
                            self.md_pending = Some(pending);
                        }
                        let pending = self.md_pending.as_ref().unwrap().clone();
                        self.md.as_mut().unwrap().push(&delta);
                        for (drained, block) in pending.borrow_mut().drain(..) {
                            if let Some(i) = cur.take() {
                                self.entries[i] = Entry::Text(drained);
                            } else if !drained.is_empty() {
                                self.entries.push(Entry::Text(drained));
                            }
                            self.push_block(block);
                            cur = None;
                        }
                        let text = self.md.as_ref().unwrap().current_text();
                        if let Some(i) = cur {
                            if !text.is_empty() {
                                self.entries[i] = Entry::Text(text);
                            }
                        } else if !text.is_empty() {
                            self.entries.push(Entry::Text(text));
                            cur = Some(self.entries.len() - 1);
                        }
                        self.activity = Activity::Streaming;
                    }
                    TurnEvent::AssistantDone => {
                        self.flush_tools();
                        if let Some(mut md) = self.md.take() {
                            let pending = self.md_pending.take();
                            let tail = md.finish();
                            if let Some(pending) = pending {
                                for (drained, block) in pending.borrow_mut().drain(..) {
                                    if !drained.is_empty() {
                                        self.entries.push(Entry::Text(drained));
                                    }
                                    self.push_block(block);
                                }
                            }
                            if !tail.is_empty() {
                                if let Some(i) = cur.take() {
                                    self.entries[i] = Entry::Text(tail);
                                } else {
                                    self.entries.push(Entry::Text(tail));
                                }
                            }
                        }
                        cur = None;
                        self.activity = Activity::Thinking;
                    }
                    TurnEvent::ToolStart { label, .. } => {
                        self.tool_running = Some(label.clone());
                        self.tool_live = None;
                    }
                    TurnEvent::ToolDelta(text) => {
                        self.tool_live = Some(text);
                    }
                    TurnEvent::ToolResult { label, .. } => {
                        self.tool_running = None;
                        self.tool_live = None;
                        self.pending_tools.push(label);
                        self.activity = Activity::Thinking;
                    }
                    TurnEvent::Tokens {
                        input,
                        output,
                        cached_input,
                    } => {
                        self.live_in = input;
                        self.live_out = output;
                        self.live_cached_in = cached_input;
                    }
                    TurnEvent::Notice(text) => {
                        self.flush_tools();
                        self.entries.push(Entry::Notice(text));
                    }
                    TurnEvent::End {
                        messages,
                        usage,
                        err,
                        cancelled,
                        compact,
                    } => {
                        self.running = false;
                        self.tool_running = None;
                        self.tool_live = None;
                        self.sess_in += usage.input;
                        self.sess_out += usage.output;
                        if let Some(mut md) = self.md.take() {
                            let pending = self.md_pending.take();
                            let tail = md.finish();
                            if let Some(pending) = pending {
                                for (drained, block) in pending.borrow_mut().drain(..) {
                                    if let Some(i) = cur.take() {
                                        self.entries[i] = Entry::Text(drained);
                                    } else if !drained.is_empty() {
                                        self.entries.push(Entry::Text(drained));
                                    }
                                    self.push_block(block);
                                    cur = None;
                                }
                            }
                            if !tail.is_empty() {
                                if let Some(i) = cur.take() {
                                    self.entries[i] = Entry::Text(tail);
                                } else {
                                    self.entries.push(Entry::Text(tail));
                                }
                            }
                        }
                        cur = None;
                        self.cur_text = None;
                        self.msgs = messages.clone();
                        self.flush_tools();
                        if cancelled {
                            self.entries.push(Entry::Notice("interrupted".into()));
                        } else if err.is_none() {
                            self.entries.push(Entry::Summary {
                                secs: self.turn_start.elapsed().as_secs(),
                                input: usage.input,
                                output: usage.output,
                            });
                        }
                        if let Some(err) = err {
                            self.entries.push(Entry::Notice(format!("error: {err}")));
                            self.persist_session(&messages, usage, Some(&err), compact);
                        } else {
                            self.persist_session(&messages, usage, None, compact);
                        }
                        self.activity = Activity::Idle;
                    }
                }
            }
            self.rx = Some(rx);
        }
        self.cur_text = cur;
        if let Some(rx) = self.compact_rx.take() {
            match rx.try_recv() {
                Ok(Ok((summary, tokens_before, retained))) => {
                    self.finish_compaction(summary, tokens_before, retained);
                }
                Ok(Err(e)) => {
                    self.compacting = false;
                    self.retry_after_compact = false;
                    self.entries
                        .push(Entry::Notice(format!("compaction failed: {e}")));
                }
                Err(_) => self.compact_rx = Some(rx),
            }
        }
        any
    }

    fn flush_tools(&mut self) {
        if self.pending_tools.is_empty() {
            return;
        }
        let calls = std::mem::take(&mut self.pending_tools);
        self.entries.push(Entry::Tool { calls });
    }

    fn push_block(&mut self, block: Block) {
        match block {
            Block::Code { language, code } => {
                self.entries.push(Entry::Code(markdown::render_code_block(
                    language.as_bytes(),
                    code.as_bytes(),
                )));
            }
            Block::Table(t) => self.entries.push(Entry::Table(t)),
            Block::Rule => self.entries.push(Entry::Rule),
        }
    }

    fn slash(&mut self, cmd: &str) {
        let name = match cmd.split_once(' ') {
            Some((n, _)) => n,
            None => cmd,
        };
        // These replace session state; running them mid-turn would clobber
        // the transcript the worker is still producing.
        if self.running && matches!(name, "new" | "reset" | "resume" | "rewind" | "compact") {
            self.entries.push(Entry::Notice(
                "agent is running; ctrl+c interrupts it first".into(),
            ));
            return;
        }
        match name {
            "help" => self.open_screen(Screen::Help),
            "new" => self.fresh_session(true),
            "reset" => self.fresh_session(false),
            "resume" => self.open_screen(Screen::Resume),
            "rewind" => self.open_screen(Screen::Rewind),
            "compact" => {
                if self.compacting {
                    self.entries
                        .push(Entry::Notice("already compacting…".into()));
                } else {
                    let entries = session::load_live(&self.cfg.session_dir);
                    if session::context_messages(&entries).len() < 4 {
                        self.entries.push(Entry::Notice(format!(
                            "{DIM}session too small to compact{RESET}"
                        )));
                    } else {
                        self.start_compaction(entries);
                    }
                }
            }
            "copy" => self.copy_last(),
            "quit" => {
                self.want_quit = true;
            }
            _ => {
                self.entries.push(Entry::Notice(format!(
                    "{DIM}unknown command: /{name}{RESET}"
                )));
            }
        }
    }

    fn fresh_session(&mut self, archive: bool) {
        if archive {
            let entries = session::load_live(&self.cfg.session_dir);
            match self.resume_id.take() {
                Some(id) => {
                    session::continue_archived(&self.cfg.session_dir, &id, &entries);
                }
                None => {
                    session::archive_live(&self.cfg.session_dir);
                }
            }
        } else {
            // /reset discards in-memory changes; the origin archive keeps
            // its state from load time.
            self.resume_id = None;
        }
        self.entries.clear();
        self.entries.push(Entry::Welcome);
        self.msgs.clear();
        self.pending_tools.clear();
        self.sess_in = 0;
        self.sess_out = 0;
        self.live_in = 0;
        self.live_out = 0;
        self.live_cached_in = 0;
        self.input.history.clear();
        self.activity = Activity::Idle;
        self.running = false;
        self.cur_text = None;
        self.md = None;
        self.md_pending = None;
        self.streamed.clear();
    }

    fn copy_last(&mut self) {
        let mut text = String::new();
        for m in self.msgs.iter().rev() {
            if m.role == "assistant" && !m.content.is_empty() {
                text = m.content.clone();
                break;
            }
        }
        if text.is_empty() {
            self.entries.push(Entry::Notice(format!(
                "{DIM}no assistant response to copy{RESET}"
            )));
            return;
        }
        let b64 = b64_encode(text.as_bytes());
        print!("\x1b]52;c;{b64}\x1b\\");
        let _ = std::io::stdout().flush();
        self.entries
            .push(Entry::Notice(format!("{DIM}copied last response{RESET}")));
    }

    fn resume_by_id(&mut self, id: &str) {
        let dir = self.cfg.session_dir.clone();
        let loaded = if id == "last" {
            session::list_sessions(&dir)
                .into_iter()
                .next()
                .map(|s| (s.id, session::load_session(&s.path)))
        } else {
            session::load_by_id(&dir, id).map(|msgs| (id.to_string(), msgs))
        };
        match loaded {
            Some((id, msgs)) => {
                self.resume_id = Some(id);
                self.load_messages(msgs);
            }
            None => {
                self.entries
                    .push(Entry::Notice(format!("{DIM}no such session: {id}{RESET}")));
            }
        }
    }

    fn load_messages(&mut self, entries: Vec<session::Entry>) {
        self.msgs = session::context_messages(&entries);
        self.sess_in = 0;
        self.sess_out = 0;
        self.live_in = 0;
        self.live_out = 0;
        self.live_cached_in = 0;
        for entry in &entries {
            if let session::Entry::Usage {
                input,
                output,
                cached_input,
                context_input,
                ..
            } = entry
            {
                self.sess_in += input;
                self.sess_out += output;
                self.live_in = *context_input;
                self.live_cached_in = *cached_input;
            }
        }
        self.entries.clear();
        self.entries.push(Entry::Welcome);
        let mut tools: Vec<String> = Vec::new();
        for m in &self.msgs {
            match m.role.as_str() {
                "user" => {
                    if !tools.is_empty() {
                        self.entries.push(Entry::Tool {
                            calls: std::mem::take(&mut tools),
                        });
                    }
                    self.entries.push(Entry::User(m.content.clone()));
                }
                "assistant" => {
                    if !m.content.is_empty() {
                        if !tools.is_empty() {
                            self.entries.push(Entry::Tool {
                                calls: std::mem::take(&mut tools),
                            });
                        }
                        self.entries
                            .push(Entry::Text(markdown::Markdown::render(&m.content)));
                    }
                    for c in &m.tool_calls {
                        tools.push(tool_label(c, false));
                    }
                }
                _ => {}
            }
        }
        if !tools.is_empty() {
            self.entries.push(Entry::Tool { calls: tools });
        }
        self.pending_tools.clear();
        self.streamed.clear();
        self.overflow_retried = false;
        self.reprint = true;
        session::save_live(&self.cfg.session_dir, &entries);
    }

    fn open_screen(&mut self, screen: Screen) {
        match screen {
            Screen::Resume => {
                self.sessions = session::list_sessions(&self.cfg.session_dir);
            }
            Screen::Rewind => {
                self.rewind_items = self.build_rewind_items();
            }
            _ => {}
        }
        self.screen = screen;
        self.input.take();
        self.sel = 0;
        self.window_start = 0;
        if screen == Screen::Rewind {
            // Start on the most recent message: rewinding usually means
            // going back just a turn or two.
            self.sel = self.filtered_rewind_items().len().saturating_sub(1);
        }
        self.last_frame.clear();
    }

    fn build_rewind_items(&self) -> Vec<RewindItem> {
        let mut out = Vec::new();
        for (i, m) in self.msgs.iter().enumerate() {
            let role = m.role.as_str();
            if role != "user" && role != "assistant" {
                continue;
            }
            if role == "user" && m.content.starts_with(session::COMPACTION_PREFIX) {
                continue;
            }
            let mut preview = m.content.lines().next().unwrap_or("").trim().to_string();
            if preview.is_empty() {
                if role == "assistant" && !m.tool_calls.is_empty() {
                    let n = m.tool_calls.len();
                    preview = format!("({n} tool call{})", if n == 1 { "" } else { "s" });
                } else {
                    continue;
                }
            }
            out.push(RewindItem {
                msg_idx: i,
                role: m.role.clone(),
                preview,
            });
        }
        out
    }

    fn filtered_rewind_items(&self) -> Vec<RewindItem> {
        let q = self.input.buf().trim().to_lowercase();
        self.rewind_items
            .iter()
            .filter(|it| {
                q.is_empty()
                    || it.preview.to_lowercase().contains(&q)
                    || it.role.to_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    }

    /// Drop everything from message `idx` onward, in memory and on disk.
    /// The session file is append-only with compaction markers, so the
    /// truncated transcript is rewritten as a flat message list; a later
    /// compaction will re-summarize as usual.
    fn rewind_to(&mut self, idx: usize) {
        let idx = idx.min(self.msgs.len());
        let entries: Vec<session::Entry> = self.msgs[..idx]
            .iter()
            .map(|message| session::Entry::Message {
                message: message.clone(),
            })
            .collect();
        self.load_messages(entries);
        let n = self.msgs.len();
        self.entries.push(Entry::Notice(format!(
            "{DIM}rewound · {n} message{} remaining{RESET}",
            if n == 1 { "" } else { "s" }
        )));
    }

    fn catalog_item_count(&self) -> usize {
        match self.screen {
            Screen::Help => self.help_items().len(),
            Screen::Resume => self.filtered_sessions().len(),
            Screen::Rewind => self.filtered_rewind_items().len(),
            _ => 0,
        }
    }

    fn help_items(&self) -> Vec<SlashItem> {
        let query = self.input.buf().trim().to_lowercase();
        SLASH
            .iter()
            .filter(|spec| {
                query.is_empty()
                    || spec.command.to_lowercase().contains(&query)
                    || spec.description.to_lowercase().contains(&query)
                    || spec.category.to_lowercase().contains(&query)
            })
            .map(SlashItem::builtin)
            .collect()
    }

    fn filtered_sessions(&self) -> Vec<&SessionMeta> {
        let q = self.input.buf().trim().to_lowercase();
        self.sessions
            .iter()
            .filter(|s| q.is_empty() || s.title.to_lowercase().contains(&q))
            .collect()
    }

    fn catalog_activate(&mut self) {
        match self.screen {
            Screen::Help => {
                let items = self.help_items();
                if let Some(spec) = items.get(self.sel) {
                    let cmd = spec.command.clone();
                    self.close_screen();
                    if let Some(rest) = cmd.strip_prefix('/') {
                        self.slash(rest);
                    }
                }
            }
            Screen::Resume => {
                let s = self.filtered_sessions().get(self.sel).cloned().cloned();
                if let Some(s) = s {
                    // Flush the session being continued so switching targets
                    // does not drop its transcript.
                    if let Some(prev) = self.resume_id.take() {
                        let cur = session::load_live(&self.cfg.session_dir);
                        session::continue_archived(&self.cfg.session_dir, &prev, &cur);
                    }
                    let msgs = session::load_session(&s.path);
                    let title = s.title.clone();
                    self.close_screen();
                    self.resume_id = Some(s.id);
                    self.load_messages(msgs);
                    self.entries
                        .push(Entry::Notice(format!("{DIM}resumed: {title}{RESET}")));
                }
            }
            Screen::Rewind => {
                let item = self.filtered_rewind_items().get(self.sel).cloned();
                if let Some(it) = item {
                    let idx = it.msg_idx;
                    self.close_screen();
                    self.rewind_to(idx);
                }
            }
            Screen::None => {}
        }
    }

    fn catalog_rows(&mut self, rows: u16, cols: u16) -> Vec<String> {
        let width = cols as usize;
        let mut out = Vec::new();
        let (composer_rows, _, _) = self.input.render_with("┃ ", width);
        let composer = composer_rows.first().cloned().unwrap_or_default();
        out.push(format!("{HINT}{composer}{RESET}"));
        out.push(format!("{DIVIDER}{}{RESET}", "\u{2500}".repeat(width)));
        let q = self.input.buf().trim().to_lowercase();
        let sel = self.sel;
        let dir = self.cfg.dir.clone();
        let screen = self.screen;
        match screen {
            Screen::Help => {
                let items = self.help_items();
                out.push(format!("{SELECTED}Commands {}{RESET}", items.len()));
                push_catalog_items(
                    &mut self.window_start,
                    sel,
                    &mut out,
                    items.len(),
                    rows,
                    |i| {
                        let s = &items[i];
                        let style = if i == sel { SELECTED } else { DIM };
                        let mut r = format!("{style}  {}{RESET}", s.help);
                        let desc_col = width * 2 / 3;
                        let pad = desc_col.saturating_sub(visible_width(&r));
                        r.push_str(&" ".repeat(pad));
                        r.push_str(&format!(
                            "{DIM}{}{RESET}",
                            clip(&s.description, width.saturating_sub(desc_col))
                        ));
                        r
                    },
                );
            }
            Screen::Resume => {
                let items: Vec<(String, i64, usize)> = self
                    .sessions
                    .iter()
                    .filter(|s| q.is_empty() || s.title.to_lowercase().contains(&q))
                    .map(|s| (s.title.clone(), s.updated, s.turns))
                    .collect();
                out.push(format!("{SELECTED}Sessions {}{RESET}", items.len()));
                push_catalog_items(
                    &mut self.window_start,
                    sel,
                    &mut out,
                    items.len(),
                    rows,
                    |i| {
                        let (title, updated, turns) = &items[i];
                        let age = age_str(*updated);
                        let meta = format!(
                            "{} · {} · {} turn{}",
                            workspace_label(&dir),
                            age,
                            turns,
                            if *turns == 1 { "" } else { "s" }
                        );
                        let desc_col = width * 2 / 3;
                        let style = if i == sel { SELECTED } else { DIM };
                        let mut r = format!(
                            "{style}  {}{RESET}",
                            clip(title, desc_col.saturating_sub(4))
                        );
                        let pad = desc_col.saturating_sub(visible_width(&r));
                        r.push_str(&" ".repeat(pad));
                        r.push_str(&format!("{DIM}{meta}{RESET}"));
                        r
                    },
                );
            }
            Screen::Rewind => {
                let items = self.filtered_rewind_items();
                out.push(format!("{SELECTED}Rewind {}{RESET}", items.len()));
                push_catalog_items(
                    &mut self.window_start,
                    sel,
                    &mut out,
                    items.len(),
                    rows,
                    |i| {
                        let it = &items[i];
                        let desc_col = width * 2 / 3;
                        let style = if i == sel { SELECTED } else { DIM };
                        let mut r = format!(
                            "{style}  {}{RESET}",
                            clip(&it.preview, desc_col.saturating_sub(4))
                        );
                        let pad = desc_col.saturating_sub(visible_width(&r));
                        r.push_str(&" ".repeat(pad));
                        r.push_str(&format!(
                            "{DIM}{}{RESET}",
                            if it.role == "user" {
                                "you"
                            } else {
                                "assistant"
                            }
                        ));
                        r
                    },
                );
            }
            Screen::None => {}
        }
        let hint = match screen {
            Screen::Help => "↑↓ Navigate     Enter Open     Esc Close",
            Screen::Resume => "↑↓ Navigate     Enter Open     Esc Close",
            Screen::Rewind => "↑↓ Navigate     Enter Rewind     Esc Close",
            Screen::None => "",
        };
        out.push(format!("{DIM}{hint}{RESET}"));
        out
    }

    // ---------- rendering ----------

    fn paint(&mut self, term: &mut Terminal) {
        let (rows, cols) = term.size();
        let resized = rows != self.rows || cols != self.cols;
        self.rows = rows;
        self.cols = cols;
        if self.screen != Screen::None {
            self.enter_alt(term);
            self.paint_catalog(term, resized);
            return;
        }
        self.paint_inline(term, resized);
    }

    fn render_transcript(&self) -> Vec<String> {
        let width = (self.cols as usize).max(1);
        let mut rows = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::Welcome => {
                    rows.push(format!(
                        "{WELCOME_APP}axe{RESET}{DIM} v{VERSION} · Run /help for commands{RESET}"
                    ));
                }
                Entry::User(text) => {
                    for line in wrap_text(text, width.saturating_sub(2)) {
                        rows.push(format!("{USER_RAIL}┃{RESET} {BOLD}{line}{RESET}"));
                    }
                }
                Entry::Text(t) => rows.extend(wrap_gutter(t, width, 2)),
                Entry::Code(c) => rows.extend(wrap_gutter(c, width, 2)),
                Entry::Table(t) => rows.extend(wrap_gutter(t, width, 2)),
                Entry::Rule => rows.push(format!(
                    "{DIM}{}{RESET}",
                    "\u{2500}".repeat(markdown::ansi::HORIZONTAL_RULE_WIDTH)
                )),
                Entry::Tool { calls } => {
                    let n = calls.len();
                    rows.push(format!(
                        "{USER_RAIL}●{RESET} {DIM}{n} tool call{}{RESET}",
                        if n == 1 { "" } else { "s" }
                    ));
                    let last = n.saturating_sub(1);
                    for (i, label) in calls.iter().enumerate() {
                        let branch = if i == last { "└" } else { "├" };
                        rows.push(format!("{DIM}{branch} {label}{RESET}"));
                    }
                }
                Entry::Notice(text) => rows.push(text.clone()),
                Entry::Summary {
                    secs,
                    input,
                    output,
                } => {
                    rows.push(format!(
                        "{DIM}  {} (↑{} ↓{}){RESET}",
                        format_dur(*secs),
                        tok(*input),
                        tok(*output)
                    ));
                }
            }
            rows.push(String::new());
        }
        if let Some(label) = &self.tool_running {
            let now = self.turn_start.elapsed();
            let half = (now.as_millis() as i64 / 500) % 2 == 0;
            let marker = if half { "●" } else { " " };
            rows.push(format!("{ACTIVITY}{marker} {label}{RESET}"));
            if let Some(live) = &self.tool_live {
                let width = self.cols.saturating_sub(4) as usize;
                let count = live.chars().count();
                let mut t: String = live.chars().take(width).collect();
                if count > width {
                    t.push('…');
                }
                rows.push(format!("{DIM}  {t}{RESET}"));
            }
        } else {
            match &self.activity {
                Activity::Thinking => {
                    let now = self.turn_start.elapsed();
                    let secs = now.as_secs();
                    let half = (now.as_millis() as i64 / 500) % 2 == 0;
                    let head = if half {
                        format!("{ACTIVITY}• Thinking ({secs}s)")
                    } else {
                        format!(" {ACTIVITY} Thinking ({secs}s)")
                    };
                    rows.push(format!(
                        "{head}{DIM} (↑{} ↓{}){RESET}",
                        tok(self.live_in),
                        tok(self.live_out)
                    ));
                }
                Activity::Streaming => {
                    if self.live_in > 0 || self.live_out > 0 {
                        rows.push(format!(
                            "{DIM}  (↑{} ↓{}){RESET}",
                            tok(self.live_in),
                            tok(self.live_out)
                        ));
                    } else {
                        rows.push(String::from("  "));
                    }
                }
                _ => {}
            }
        }
        rows
    }

    fn paint_inline(&mut self, term: &mut Terminal, resized: bool) {
        let out = term.out();
        if !self.painted_once {
            let _ = out.write_all(term::clear_display().as_bytes());
            self.painted_once = true;
            self.streamed.clear();
        }
        let content = self.render_transcript();
        self.sync_picker();
        let (chrome, cursor_row, cursor_col) = self.chrome_rows();
        let rows = (self.rows as usize).max(1);
        let capacity = rows.saturating_sub(chrome.len()).max(1);
        if std::mem::take(&mut self.reprint) {
            if content.len() > capacity {
                for line in &content {
                    let _ = write!(out, "{}", term::move_to(rows as u16, 1));
                    let _ = writeln!(out, "{line}");
                }
                self.streamed = content.clone();
                self.last_capacity = capacity;
                self.last_chrome = None;
            } else {
                self.repaint_tail(out, &content, capacity);
                self.streamed = content.clone();
                self.last_capacity = capacity;
            }
        } else if capacity != self.last_capacity {
            self.repaint_tail(out, &content, capacity);
            self.streamed = content.to_vec();
            self.last_capacity = capacity;
        } else {
            self.update_content(out, &content, rows, capacity);
        }
        let vis = content.len().min(capacity);
        self.last_input_row = (vis + 1) as u16;
        let same_chrome = !resized
            && self
                .last_chrome
                .as_ref()
                .map(|(v, c, r, col)| {
                    *v == vis && c == &chrome && *r == cursor_row && *col == cursor_col
                })
                .unwrap_or(false);
        if !same_chrome {
            for row in (vis + 1)..=rows {
                let _ = write!(out, "{}", term::move_to(row as u16, 1));
                let _ = out.write_all(term::clear_eol().as_bytes());
            }
            for (i, line) in chrome.iter().enumerate() {
                let _ = write!(out, "{}", term::move_to((vis + 1 + i) as u16, 1));
                let _ = out.write_all(line.as_bytes());
            }
            let _ = write!(
                out,
                "{}",
                term::move_to((vis + 1 + cursor_row) as u16, cursor_col as u16)
            );
            let _ = out.write_all(term::cursor_visible().as_bytes());
            self.last_chrome = Some((vis, chrome, cursor_row, cursor_col));
        }
        let _ = out.flush();
    }

    fn update_content(
        &mut self,
        out: &mut std::io::Stdout,
        new: &[String],
        rows: usize,
        capacity: usize,
    ) {
        let old = &self.streamed;
        if new == old {
            return;
        }
        let old_vis = old.len().min(capacity);
        let new_vis = new.len().min(capacity);
        let old_scrolled = old.len().saturating_sub(old_vis);
        let new_scrolled = new.len().saturating_sub(new_vis);
        let jump = new_scrolled.saturating_sub(old_scrolled);
        let mut d = 0;
        while d < old.len() && d < new.len() && old[d] == new[d] {
            d += 1;
        }
        if jump > capacity && old.len().saturating_sub(d) <= 2 {
            for line in &new[d..] {
                let _ = write!(out, "{}", term::move_to(rows as u16, 1));
                let _ = writeln!(out, "{}", line);
            }
            // Scrolling the screen shifted the chrome rows too; force a
            // chrome repaint below.
            self.last_chrome = None;
            self.streamed = new.to_vec();
            return;
        }
        // Patch path: all differences inside the visible window and the
        // content did not shrink below the previous scroll point.
        if new_scrolled >= old_scrolled {
            let mut all_in_window = true;
            let mut changed = false;
            for i in 0..new.len().max(old.len()) {
                if old.get(i) != new.get(i) {
                    changed = true;
                    if i < new_scrolled {
                        all_in_window = false;
                        break;
                    }
                }
            }
            if changed && all_in_window {
                if new_scrolled > old_scrolled {
                    if new_scrolled > old.len() {
                        self.repaint_tail(out, new, capacity);
                        self.streamed = new.to_vec();
                        return;
                    }
                    for _ in 0..(new_scrolled - old_scrolled) {
                        let _ = write!(out, "{}", term::move_to(rows as u16, 1));
                        let _ = out.write_all(b"\n");
                    }
                    // The newlines scrolled the chrome rows up as well;
                    // force a chrome repaint below.
                    self.last_chrome = None;
                }
                for i in 0..new.len() {
                    if old.get(i) != new.get(i) {
                        let row = (i - new_scrolled + 1) as u16;
                        let _ = write!(out, "{}", term::move_to(row, 1));
                        let _ = out.write_all(term::clear_eol().as_bytes());
                        let _ = out.write_all(new[i].as_bytes());
                    }
                }
                for i in new.len()..old.len() {
                    if i >= new_scrolled {
                        let row = (i - new_scrolled + 1) as u16;
                        let _ = write!(out, "{}", term::move_to(row, 1));
                        let _ = out.write_all(term::clear_eol().as_bytes());
                    }
                }
                self.streamed = new.to_vec();
                return;
            }
        }
        self.repaint_tail(out, new, capacity);
        self.streamed = new.to_vec();
    }

    fn repaint_tail(&self, out: &mut std::io::Stdout, new: &[String], capacity: usize) {
        let start = new.len().saturating_sub(capacity);
        for i in 0..capacity {
            let _ = write!(out, "{}", term::move_to((i + 1) as u16, 1));
            let _ = out.write_all(term::clear_eol().as_bytes());
            if let Some(r) = new.get(start + i) {
                let _ = out.write_all(r.as_bytes());
            }
        }
    }

    fn hint_line(&self, scroll_hint: Option<&str>) -> String {
        if self.ctrl_c_pending {
            return format!("{DIM}press ctrl+c again to exit{RESET}");
        }
        let mut segs: Vec<String> = Vec::new();
        if self.cfg.api_key.is_empty() {
            segs.push(format!("{DIM}no api key: set OPENAI_API_KEY{RESET}"));
        }
        segs.push(self.model_display.clone());
        if let Some(window) = self.cfg.context_window
            && window > 0
        {
            let percent = self.live_in.saturating_mul(100) / window;
            if self.cols <= 60 {
                segs.push(format!("{percent}%"));
            } else {
                segs.push(format!(
                    "{}/{} ({percent}%)",
                    tok(self.live_in),
                    tok(window)
                ));
            }
        }
        if self.cols > 60 && self.live_cached_in > 0 {
            let percent = self
                .live_cached_in
                .saturating_mul(100)
                .checked_div(self.live_in)
                .unwrap_or(0);
            segs.push(format!("cache {} ({percent}%)", tok(self.live_cached_in)));
        }
        if self.cols > 60 {
            segs.push(format!("↑{} ↓{}", tok(self.sess_in), tok(self.sess_out)));
        }
        if let Some(extra) = scroll_hint {
            segs.push(extra.to_string());
        }
        format!("{DIM}{}{RESET}", segs.join(" · "))
    }

    fn composer_prefix(&self) -> String {
        format!("{USER_RAIL}┃{RESET} ")
    }

    fn chrome_rows(&self) -> (Vec<String>, usize, usize) {
        self.chrome_rows_with_hint(None)
    }

    fn chrome_rows_with_hint(&self, scroll_hint: Option<&str>) -> (Vec<String>, usize, usize) {
        let (input_rows, cursor_row, cursor_col) = self
            .input
            .render_with(&self.composer_prefix(), self.cols as usize);
        let cap = (self.rows as usize / 2).max(4);
        let mut vis_start = input_rows.len().saturating_sub(cap);
        if cursor_row < vis_start {
            vis_start = cursor_row;
        }
        let mut rows: Vec<String> = input_rows[vis_start..].to_vec();
        let vis_cursor_row = cursor_row - vis_start;
        if let Some(p) = &self.picker {
            let width = self.cols as usize;
            rows.push(picker_divider(width));
            match p.kind {
                PickerKind::Slash => {
                    rows.push(slash_header(p, width));
                    rows.push(String::new());
                    let n = p.slash_matches.len();
                    for idx in p.win..(p.win + PICKER_VISIBLE).min(n) {
                        rows.push(slash_row(&p.slash_matches[idx], idx == p.sel, width));
                    }
                }
                PickerKind::Files => {
                    let n = p.file_matches.len();
                    for idx in p.win..(p.win + PICKER_VISIBLE).min(n) {
                        rows.push(file_row(
                            &p.file_matches[idx],
                            &p.query,
                            idx == p.sel,
                            width,
                        ));
                    }
                }
            }
            rows.push(picker_divider(width));
        }
        if self.cols > 60 {
            rows.push(String::new());
        }
        rows.push(self.hint_line(scroll_hint));
        (rows, vis_cursor_row, cursor_col)
    }

    fn paint_catalog(&mut self, term: &mut Terminal, resized: bool) {
        self.enter_alt(term);
        if resized {
            self.last_frame.clear();
        }
        let out = term.out();
        if self.last_frame.is_empty() {
            let _ = out.write_all(term::clear_display().as_bytes());
        }
        let frame = self.catalog_rows(self.rows, self.cols);
        self.emit_diff(out, &frame);
        let _ = out.write_all(term::cursor_hidden().as_bytes());
        let _ = out.flush();
    }

    fn emit_diff(&mut self, out: &mut std::io::Stdout, frame: &[String]) {
        for (i, row) in frame.iter().enumerate() {
            let prev = self.last_frame.get(i).map(|s| s.as_str()).unwrap_or("");
            if prev != row {
                let _ = write!(out, "{}", term::move_to((i + 1) as u16, 1));
                let _ = out.write_all(term::clear_eol().as_bytes());
                let _ = out.write_all(row.as_bytes());
            }
        }
        if self.last_frame.len() > frame.len() {
            for i in frame.len()..self.last_frame.len() {
                let _ = write!(out, "{}", term::move_to((i + 1) as u16, 1));
                let _ = out.write_all(term::clear_eol().as_bytes());
            }
        }
        self.last_frame = frame.to_vec();
    }

    fn picker_enter(&mut self) {
        let Some(p) = self.picker.take() else { return };
        self.picker_dismissed = None;
        match p.kind {
            PickerKind::Slash => {
                if let Some(spec) = p.slash_matches.get(p.sel) {
                    let cmd = spec.command.trim_start_matches('/');
                    self.input.take();
                    self.slash(cmd);
                }
            }
            PickerKind::Files => {
                if let Some(path) = p.file_matches.get(p.sel) {
                    let s = p.token_start + 1;
                    let e = p.token_end;
                    self.input.replace_range(s, e, path);
                    if !path.ends_with('/') {
                        self.picker_dismissed = Some(PickerKind::Files);
                    }
                }
            }
        }
    }

    fn picker_tab(&mut self) {
        let Some(p) = &self.picker else { return };
        match p.kind {
            PickerKind::Slash => {
                if let Some(spec) = p.slash_matches.get(p.sel) {
                    let s = p.token_start;
                    let e = p.token_end;
                    let cmd = spec.command.to_string();
                    self.input.replace_range(s, e, &cmd);
                }
            }
            PickerKind::Files => {
                self.picker_enter();
            }
        }
    }

    fn picker_move(&mut self, up: bool) {
        let Some(p) = &mut self.picker else { return };
        let n = match p.kind {
            PickerKind::Slash => p.slash_matches.len(),
            PickerKind::Files => p.file_matches.len(),
        };
        if n == 0 {
            return;
        }
        if up {
            p.sel = (p.sel + n - 1) % n;
        } else {
            p.sel = (p.sel + 1) % n;
        }
        if p.sel < p.win {
            p.win = p.sel;
        }
        if p.sel >= p.win + PICKER_VISIBLE {
            p.win = p.sel - PICKER_VISIBLE + 1;
        }
    }

    fn picker_dismiss(&mut self) {
        if let Some(p) = &self.picker {
            self.picker_dismissed = Some(p.kind);
        }
        self.picker = None;
    }

    fn sync_picker(&mut self) {
        if self.screen != Screen::None {
            self.picker = None;
            return;
        }
        let trigger = self.picker_trigger();
        match trigger {
            None => {
                self.picker = None;
                self.picker_dismissed = None;
            }
            Some((kind, query, token_start, token_end)) => {
                if self.picker_dismissed == Some(kind) {
                    self.picker = None;
                    return;
                }
                let same = self
                    .picker
                    .as_ref()
                    .map(|p| p.kind == kind && p.query == query)
                    .unwrap_or(false);
                if same {
                    if let Some(p) = &mut self.picker {
                        p.token_start = token_start;
                        p.token_end = token_end;
                    }
                    return;
                }
                let mut p = Picker {
                    kind,
                    token_start,
                    token_end,
                    query,
                    sel: 0,
                    win: 0,
                    slash_matches: Vec::new(),
                    file_matches: Vec::new(),
                };
                match p.kind {
                    PickerKind::Slash => p.slash_matches = slash_matches(&p.query),
                    PickerKind::Files => p.file_matches = file_matches(&p.query, &self.cfg.dir),
                }
                self.picker = Some(p);
            }
        }
    }

    fn picker_trigger(&self) -> Option<(PickerKind, String, usize, usize)> {
        let input = self.input.buf();
        let chars: Vec<char> = input.chars().collect();
        let cursor = self.input.cursor.min(chars.len());
        let mut i = cursor;
        while i > 0 {
            let c = chars[i - 1];
            if c == '@' {
                let at = i - 1;
                if at == 0 || is_file_boundary(chars[at - 1]) {
                    let q: String = chars[i..cursor].iter().collect();
                    return Some((PickerKind::Files, q, at, cursor));
                }
            }
            if is_picker_term(c) {
                break;
            }
            i -= 1;
        }
        let mut j = cursor;
        while j > 0 && !is_picker_term(chars[j - 1]) {
            j -= 1;
        }
        if j < cursor
            && chars.get(j) == Some(&'/')
            && j + 1 < cursor
            && !input[..j].trim().is_empty()
        {
            let q: String = chars[j + 1..cursor].iter().collect();
            return Some((PickerKind::Slash, q, j, cursor));
        }
        let start = input.len() - input.trim_start_matches([' ', '\t', '\r', '\n']).len();
        if chars.get(start) == Some(&'/') {
            let q: String = chars[start + 1..cursor].iter().collect();
            if !q.chars().any(is_picker_term) {
                return Some((PickerKind::Slash, q, start, cursor));
            }
        }
        None
    }
}

fn push_catalog_items(
    window_start: &mut usize,
    sel: usize,
    out: &mut Vec<String>,
    count: usize,
    rows: u16,
    row_fn: impl Fn(usize) -> String,
) {
    let header = 3usize;
    let hint = 1usize;
    let available = (rows as usize).saturating_sub(header + hint).max(1);
    let sel = sel.min(count.saturating_sub(1));
    let mut start = *window_start;
    if sel < start {
        start = sel;
    }
    if sel >= start + available {
        start = sel + 1 - available;
    }
    *window_start = start;
    for i in start..(start + available).min(count) {
        out.push(row_fn(i));
    }
}

#[derive(Default)]
struct Input {
    buf: String,
    cursor: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
    preferred_col: Option<usize>,
}
impl Input {
    fn buf(&self) -> &str {
        &self.buf
    }

    fn take(&mut self) -> String {
        let v = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.preferred_col = None;
        self.hist_idx = None;
        v
    }

    fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += 1;
        self.preferred_col = None;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buf.remove(self.cursor);
        }
        self.preferred_col = None;
    }

    fn delete(&mut self) {
        if self.cursor < self.buf.chars().count() {
            self.buf.remove(self.cursor);
        }
        self.preferred_col = None;
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.preferred_col = None;
    }

    fn move_right(&mut self) {
        if self.cursor < self.buf.chars().count() {
            self.cursor += 1;
        }
        self.preferred_col = None;
    }

    fn move_word_left(&mut self) {
        let chars: Vec<char> = self.buf.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && chars[i - 1] != ' ' {
            i -= 1;
        }
        self.cursor = i;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = self.buf.chars().collect();
        let n = chars.len();
        let mut i = self.cursor;
        while i < n && chars[i] == ' ' {
            i += 1;
        }
        while i < n && chars[i] != ' ' {
            i += 1;
        }
        self.cursor = i;
    }

    fn home(&mut self) {
        let (s, _) = self.line_bounds();
        self.cursor = s;
        self.preferred_col = None;
    }

    fn end(&mut self) {
        let (_, e) = self.line_bounds();
        self.cursor = e;
        self.preferred_col = None;
    }

    fn doc_home(&mut self) {
        self.cursor = 0;
        self.preferred_col = None;
    }

    fn doc_end(&mut self) {
        self.cursor = self.buf.chars().count();
        self.preferred_col = None;
    }

    fn cursor_line_col(&self) -> (usize, usize) {
        let chars: Vec<char> = self.buf.chars().collect();
        let mut line = 0;
        let mut col = 0;
        for (i, &c) in chars.iter().enumerate() {
            if i >= self.cursor {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    fn line_bounds(&self) -> (usize, usize) {
        let chars: Vec<char> = self.buf.chars().collect();
        let n = chars.len();
        let cur = self.cursor.min(n);
        let mut start = 0;
        for (i, &c) in chars[..cur].iter().enumerate() {
            if c == '\n' {
                start = i + 1;
            }
        }
        let mut end = chars.len();
        for (i, &c) in chars[cur..].iter().enumerate() {
            if c == '\n' {
                end = cur + i;
                break;
            }
        }
        (start, end)
    }

    fn move_line_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }
        if self.preferred_col.is_none() {
            self.preferred_col = Some(col);
        }
        let (start, _) = self.line_bounds();
        let chars: Vec<char> = self.buf.chars().collect();
        let sep = start.saturating_sub(1);
        let mut prev_start = 0;
        for i in (0..sep).rev() {
            if chars[i] == '\n' {
                prev_start = i + 1;
                break;
            }
        }
        let prev_len = sep.saturating_sub(prev_start);
        let target = self.preferred_col.unwrap_or(col).min(prev_len);
        self.cursor = prev_start + target;
    }

    fn move_line_down(&mut self) {
        let (_, col) = self.cursor_line_col();
        if self.preferred_col.is_none() {
            self.preferred_col = Some(col);
        }
        let (_, end) = self.line_bounds();
        let chars: Vec<char> = self.buf.chars().collect();
        if end >= chars.len() || chars[end] != '\n' {
            return;
        }
        let next_start = end + 1;
        let mut next_end = chars.len();
        for (i, &c) in chars[next_start..].iter().enumerate() {
            if c == '\n' {
                next_end = next_start + i;
                break;
            }
        }
        let next_len = next_end - next_start;
        let target = self.preferred_col.unwrap_or(col).min(next_len);
        self.cursor = next_start + target;
    }

    fn replace_range(&mut self, start: usize, end: usize, rep: &str) {
        let chars: Vec<char> = self.buf.chars().collect();
        let mut out: String = chars[..start].iter().collect();
        out.push_str(rep);
        out.extend(chars[end..].iter());
        self.buf = out;
        self.cursor = start + rep.chars().count();
        self.preferred_col = None;
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            Some(i) if i > 0 => i - 1,
            Some(_) => return,
            None => self.history.len() - 1,
        };
        self.hist_idx = Some(idx);
        self.buf = self.history[idx].clone();
        self.cursor = self.buf.chars().count();
    }

    fn history_next(&mut self) {
        match self.hist_idx {
            Some(i) if i + 1 < self.history.len() => {
                self.hist_idx = Some(i + 1);
                self.buf = self.history[i + 1].clone();
                self.cursor = self.buf.chars().count();
            }
            Some(_) => {
                self.hist_idx = None;
                self.buf.clear();
                self.cursor = 0;
            }
            None => {}
        }
    }

    fn esc(&mut self) {
        self.hist_idx = None;
    }

    fn paste(&mut self, bytes: &[u8]) {
        let s = String::from_utf8_lossy(bytes);
        for c in s.chars() {
            if c == '\r' {
                self.insert('\n');
                continue;
            }
            self.insert(c);
        }
    }

    fn delete_word_left(&mut self) {
        let chars: Vec<char> = self.buf.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && chars[i - 1] != ' ' {
            i -= 1;
        }
        self.buf = chars[..i]
            .iter()
            .chain(chars[self.cursor..].iter())
            .collect();
        self.cursor = i;
    }

    fn render_with(&self, prefix: &str, width: usize) -> (Vec<String>, usize, usize) {
        let pwidth = visible_width(prefix);
        let avail = width.saturating_sub(pwidth).max(1);
        let (line_idx, col_in_line) = self.cursor_line_col();
        let mut rows = Vec::new();
        let mut cursor_row = 0usize;
        let mut cursor_col = 1usize;
        for (i, line) in self.buf.split('\n').enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let wrapped = wrap_chars(&chars, avail);
            if i == line_idx {
                let (sub, colw) = line_display_pos(&chars, col_in_line, avail);
                cursor_row = rows.len() + sub;
                cursor_col = pwidth + 1 + colw;
            }
            for r in wrapped {
                rows.push(format!("{prefix}{}", r.iter().collect::<String>()));
            }
        }
        if rows.is_empty() {
            rows.push(prefix.to_string());
        }
        (rows, cursor_row, cursor_col)
    }
}

fn wrap_chars(chars: &[char], width: usize) -> Vec<Vec<char>> {
    if width == 0 {
        return vec![chars.to_vec()];
    }
    let mut rows = Vec::new();
    let mut cur = Vec::new();
    let mut w = 0usize;
    for &c in chars {
        let cw = char_width(c);
        if w + cw > width && !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    rows.push(cur);
    rows
}

fn line_display_pos(chars: &[char], char_idx: usize, avail: usize) -> (usize, usize) {
    let a = avail.max(1);
    let w: usize = chars[..char_idx].iter().map(|c| char_width(*c)).sum();
    if char_idx == chars.len() && !chars.is_empty() {
        let total: usize = chars.iter().map(|c| char_width(*c)).sum();
        let sub = total / a;
        let sub = if total.is_multiple_of(a) {
            sub.saturating_sub(1)
        } else {
            sub
        };
        (sub, total - sub * a)
    } else {
        (w / a, w % a)
    }
}

fn slash_matches(query: &str) -> Vec<SlashItem> {
    let query = query.to_lowercase();
    SLASH
        .iter()
        .filter(|spec| query.is_empty() || spec.command[1..].to_lowercase().contains(&query))
        .map(SlashItem::builtin)
        .collect()
}

fn file_matches(query: &str, dir: &str) -> Vec<String> {
    let root = if dir.is_empty() { "." } else { dir };
    if let Some(slash) = query.rfind('/') {
        let (d, prefix) = query.split_at(slash + 1);
        let base = if d.is_empty() {
            root.to_string()
        } else if d == "/" {
            "/".to_string()
        } else {
            format!("{}/{}", root.trim_end_matches('/'), d.trim_end_matches('/'))
        };
        let mut names: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if name.starts_with('.') && is_dir {
                    continue;
                }
                if name.to_lowercase().starts_with(&prefix.to_lowercase()) {
                    names.push(format!("{d}{name}"));
                }
            }
        }
        names.sort();
        return names;
    }
    let mut pref: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut visited = 0usize;
    walk_files(root, "", query, &mut pref, &mut rest, &mut visited);
    pref.sort();
    rest.sort();
    pref.extend(rest);
    pref
}

fn walk_files(
    dir: &str,
    rel: &str,
    query: &str,
    pref: &mut Vec<String>,
    rest: &mut Vec<String>,
    visited: &mut usize,
) {
    if *visited > 4000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        *visited += 1;
        if *visited > 4000 {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if name.starts_with('.') && is_dir {
            continue;
        }
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if name == "target" || name == "node_modules" {
                continue;
            }
            walk_files(
                &format!("{}/{}", dir.trim_end_matches('/'), name),
                &path,
                query,
                pref,
                rest,
                visited,
            );
        }
        let lower = path.to_lowercase();
        let q = query.to_lowercase();
        let base = name.to_lowercase();
        let matched =
            q.is_empty() || base.starts_with(&q) || lower.starts_with(&q) || lower.contains(&q);
        if !matched {
            continue;
        }
        let display = if is_dir { format!("{path}/") } else { path };
        if q.is_empty() || base.starts_with(&q) || lower.starts_with(&q) {
            pref.push(display);
        } else {
            rest.push(display);
        }
    }
}

fn is_picker_term(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn is_file_boundary(c: char) -> bool {
    is_picker_term(c) || matches!(c, '(' | '[' | '{' | '<' | '\'' | '"' | '`')
}

fn picker_divider(width: usize) -> String {
    format!("{DIVIDER}{}{RESET}", "\u{2500}".repeat(width))
}

fn slash_header(p: &Picker, width: usize) -> String {
    let n = p.slash_matches.len();
    let mut h = format!("{DIM}Commands {n}");
    if p.query.is_empty() {
        h.push_str(" · Type to filter");
    }
    h.push_str(RESET);
    if n > PICKER_VISIBLE {
        let end = (p.win + PICKER_VISIBLE).min(n);
        let ind = format!("{}–{}", p.win + 1, end);
        let pad = width.saturating_sub(visible_width(&h) + visible_width(&ind));
        if pad > 0 {
            h.push_str(&format!("{DIM}{}{ind}{RESET}", " ".repeat(pad)));
        }
    }
    h
}

fn truncate_wide(s: &str, width: usize) -> String {
    if visible_width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > width.saturating_sub(1) {
            out.push('\u{2026}');
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

fn slash_row(spec: &SlashItem, sel: bool, width: usize) -> String {
    let cmd_col = 24usize;
    let cmd_part = format!("  {}", spec.command);
    let pad = cmd_col.saturating_sub(visible_width(&cmd_part));
    let cat = format!("  {}", spec.category);
    let desc_avail = width
        .saturating_sub(cmd_col + visible_width(&cat) + 1)
        .max(1);
    let desc = truncate_wide(&spec.description, desc_avail);
    let left = format!("{cmd_part}{}", " ".repeat(pad));
    let gap = width
        .saturating_sub(cmd_col + visible_width(&desc) + visible_width(&cat))
        .max(1);
    if sel {
        format!(
            "{BOLD}{USER_RAIL}{left}{RESET}{DIM}{desc}{RESET}{}{cat}{RESET}",
            " ".repeat(gap)
        )
    } else {
        format!("{DIM}{left}{desc}{}{cat}{RESET}", " ".repeat(gap))
    }
}

fn file_row(path: &str, query: &str, sel: bool, width: usize) -> String {
    let mut chars: Vec<char> = path.chars().collect();
    if chars.len() > 100 {
        chars.truncate(100);
    }
    let mut idx = 0usize;
    let q: Vec<char> = query.chars().collect();
    if !q.is_empty() {
        let path_str: String = chars.iter().collect();
        if let Some(p) = path_str.to_lowercase().find(&query.to_lowercase()) {
            idx = path_str[..p].chars().count();
        }
    }
    let mut hit_end = (idx + q.len()).min(chars.len());
    if path.ends_with('/') && idx + q.len() == chars.len().saturating_sub(1) {
        hit_end = chars.len();
    }
    let head: String = chars[..idx].iter().collect();
    let hit: String = chars[idx..hit_end].iter().collect();
    let tail: String = chars[hit_end..].iter().collect();
    let body = format!("  {head}");
    let avail = width.saturating_sub(visible_width(&body) + 1);
    let tail = truncate_wide(&tail, avail);
    if sel {
        format!("  {BOLD}{USER_RAIL}{head}\x1b[48;5;239m{hit}\x1b[49m{tail}{RESET}")
    } else {
        format!("  {DIM}{head}{BOLD}{hit}{RESET}{DIM}{tail}{RESET}")
    }
}

fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut w = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = ansi_seq_end(bytes, i);
            continue;
        }
        let len = utf8_len(bytes[i]);
        let ch = &s[i..i + len];
        w += char_width(ch.chars().next().unwrap_or(' '));
        i += len;
    }
    w
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut cur = String::new();
        let mut w = 0usize;
        for c in line.chars() {
            let cw = char_width(c);
            if w + cw > width && !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(c);
            w += cw;
        }
        rows.push(cur);
    }
    rows
}

fn char_width(c: char) -> usize {
    if c.is_ascii() {
        1
    } else {
        let cp = c as u32;
        if (0x1100..=0x115f).contains(&cp)
            || (0x2e80..=0xa4cf).contains(&cp)
            || (0xac00..=0xd7a3).contains(&cp)
            || (0xf900..=0xfaff).contains(&cp)
            || (0xfe30..=0xfe4f).contains(&cp)
            || (0xff00..=0xff60).contains(&cp)
            || (0x1f300..=0x1f64f).contains(&cp)
            || (0x1f900..=0x1f9ff).contains(&cp)
            || (0x20000..=0x2fffd).contains(&cp)
        {
            2
        } else {
            1
        }
    }
}

fn ansi_seq_end(bytes: &[u8], start: usize) -> usize {
    if start >= bytes.len() || bytes[start] != 0x1b {
        return start + 1;
    }
    let mut i = start + 1;
    if i < bytes.len() && bytes[i] == b'[' {
        i += 1;
        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
            i += 1;
        }
        return (i + 1).min(bytes.len());
    }
    if i < bytes.len() && bytes[i] == b']' {
        i += 1;
        while i < bytes.len() {
            if bytes[i] == 0x07 {
                return i + 1;
            }
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                return i + 2;
            }
            i += 1;
        }
        return bytes.len();
    }
    start + 1
}

fn sgr_kind(seq: &str) -> Option<&'static str> {
    if !seq.starts_with("\x1b[") || !seq.ends_with('m') {
        return None;
    }
    let body = &seq[2..seq.len() - 1];
    Some(match body {
        "0" => "reset",
        "1" => "bold",
        "2" => "dim",
        "3" => "italic",
        "4" => "underline",
        "9" => "strike",
        "22" => "bold-off",
        "23" => "italic-off",
        "24" => "underline-off",
        "29" => "strike-off",
        "39" => "fg-off",
        _ if body.starts_with("38;") => "fg",
        _ => "other",
    })
}

fn wrap_gutter(text: &str, width: usize, gutter: usize) -> Vec<String> {
    let pad = " ".repeat(gutter);
    wrap_ansi(text, width.saturating_sub(gutter).max(1))
        .into_iter()
        .map(|r| if r.is_empty() { r } else { format!("{pad}{r}") })
        .collect()
}

pub fn wrap_ansi(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut rows: Vec<String> = Vec::new();
    let text = text.strip_suffix('\n').unwrap_or(text);
    for part in text.split('\n') {
        let part = part.strip_suffix('\r').unwrap_or(part);
        if part.is_empty() {
            rows.push(String::new());
            continue;
        }
        wrap_ansi_line(&mut rows, part.to_string(), width);
    }
    rows
}

fn wrap_ansi_line(rows: &mut Vec<String>, text: String, width: usize) {
    let mut cur = String::new();
    let mut cur_width = 0usize;
    let mut active: Vec<&'static str> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let end = ansi_seq_end(bytes, i);
            let seq = &text[i..end];
            if let Some(kind) = sgr_kind(seq) {
                match kind {
                    "reset" => active.clear(),
                    "bold" | "dim" | "italic" | "underline" | "strike" | "fg" => {
                        if !active.contains(&kind) {
                            active.push(kind);
                        }
                    }
                    "bold-off" => active.retain(|k| *k != "bold"),
                    "italic-off" => active.retain(|k| *k != "italic"),
                    "underline-off" => active.retain(|k| *k != "underline"),
                    "strike-off" => active.retain(|k| *k != "strike"),
                    "fg-off" => active.retain(|k| *k != "fg"),
                    _ => {}
                }
            }
            cur.push_str(seq);
            i = end;
            continue;
        }
        let len = utf8_len(bytes[i]);
        let ch = &text[i..i + len];
        let w = char_width(ch.chars().next().unwrap_or(' '));
        if cur_width + w > width && cur_width > 0 {
            rows.push(std::mem::take(&mut cur));
            for kind in &active {
                cur.push_str(sgr_for(kind));
            }
            cur_width = 0;
        }
        cur.push_str(ch);
        cur_width += w;
        i += len;
    }
    rows.push(cur);
}

fn sgr_for(kind: &str) -> &'static str {
    match kind {
        "bold" => "\x1b[1m",
        "dim" => "\x1b[2m",
        "italic" => "\x1b[3m",
        "underline" => "\x1b[4m",
        "strike" => "\x1b[9m",
        "fg" => "\x1b[38;5;250m",
        _ => "",
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn compact_model_label(model: &str) -> String {
    let bare = model.rsplit('/').next().unwrap_or(model);
    if let Some(rest) = bare.strip_prefix("claude-") {
        for (prefix, label) in [
            ("opus-", "opus "),
            ("sonnet-", "sonnet "),
            ("haiku-", "haiku "),
        ] {
            if let Some(tail) = rest.strip_prefix(prefix) {
                return format!("{label}{tail}");
            }
        }
        return rest.to_string();
    }
    bare.to_string()
}

fn clip(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let head: String = chars.into_iter().take(n).collect();
    format!("{head}…")
}

fn format_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn tok(n: usize) -> String {
    if n < 1000 {
        return n.to_string();
    }
    format!("{:.1}k", n as f64 / 1000.0)
}

fn age_str(updated_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let delta = (now - updated_ms).max(0) as u64;
    let min = 60_000u64;
    let hour = 3_600_000u64;
    let day = 86_400_000u64;
    if delta < min {
        "now".into()
    } else if delta < hour {
        format!("{}m", delta / min)
    } else if delta < day {
        format!("{}h", delta / hour)
    } else {
        format!("{}d", delta / day)
    }
}

fn workspace_label(dir: &str) -> String {
    let d = if dir.is_empty() { "." } else { dir };
    std::path::Path::new(d)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| d.to_string())
}

fn b64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(T[(b[0] >> 2) as usize] as char);
        out.push(T[(((b[0] & 3) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b[1] & 15) << 2) | (b[2] >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(b[2] & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn build_tools(dir: &str) -> Vec<Tool> {
    vec![
        crate::tools::read(),
        crate::tools::write(),
        crate::tools::edit(),
        crate::tools::bash(dir),
    ]
}

fn tool_kind(call: &ToolCall) -> String {
    match call.name.as_str() {
        "bash" => "command".into(),
        "read" => "read".into(),
        "write" => "write".into(),
        "edit" => "edit".into(),
        _ => "command".into(),
    }
}

fn tool_label(call: &ToolCall, running: bool) -> String {
    #[derive(serde::Deserialize)]
    struct Args {
        path: Option<String>,
        command: Option<String>,
    }
    let args: Option<Args> = serde_json::from_str(&call.arguments).ok();
    let path = args
        .as_ref()
        .and_then(|a| a.path.clone())
        .unwrap_or_default();
    let command = args
        .as_ref()
        .and_then(|a| a.command.clone())
        .unwrap_or_default();
    match call.name.as_str() {
        "bash" => {
            let cmd = command.split_whitespace().collect::<Vec<_>>().join(" ");
            let cmd = clip(&cmd, 120);
            if running {
                format!("Running {cmd}")
            } else {
                format!("Ran {cmd}")
            }
        }
        "read" => format!("{} {path}", if running { "Reading" } else { "Read" }),
        "write" => format!("{} {path}", if running { "Writing" } else { "Wrote" }),
        "edit" => format!("{} {path}", if running { "Editing" } else { "Edited" }),
        _ => format!("Working: {}", call.name),
    }
}

struct TuiSink<'a> {
    tx: &'a Sender<TurnEvent>,
    steer: Receiver<String>,
    threshold: Option<usize>,
}

impl Sink for TuiSink<'_> {
    fn assistant_delta(&mut self, text: &str) {
        let _ = self.tx.send(TurnEvent::AssistantDelta(text.to_string()));
    }

    fn assistant_done(&mut self) {
        let _ = self.tx.send(TurnEvent::AssistantDone);
    }

    fn tool_start(&mut self, call: &ToolCall) {
        let _ = self.tx.send(TurnEvent::ToolStart {
            label: tool_label(call, true),
            kind: tool_kind(call),
        });
    }

    fn tool_delta(&mut self, _call: &ToolCall, text: &str) {
        let _ = self.tx.send(TurnEvent::ToolDelta(text.to_string()));
    }

    fn tool_result(&mut self, call: &ToolCall) {
        let _ = self.tx.send(TurnEvent::ToolResult {
            label: tool_label(call, false),
            kind: tool_kind(call),
        });
    }

    fn tokens(&mut self, input: usize, output: usize, cached_input: usize) {
        let _ = self.tx.send(TurnEvent::Tokens {
            input,
            output,
            cached_input,
        });
    }

    fn should_compact(&mut self, input: usize, output: usize) -> bool {
        self.threshold
            .is_some_and(|threshold| input.saturating_add(output) > threshold)
    }

    fn pending_user_input(&mut self) -> Option<String> {
        self.steer.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_compact_small_session_shows_notice() {
        let dir = std::env::temp_dir().join(format!("axe-compact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = TuiConfig {
            base: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            system: String::new(),
            dir: String::new(),
            axe_root: dir.to_str().unwrap().to_string(),
            session_dir: dir.to_str().unwrap().to_string(),
            api_key: "k".into(),
            resume: None,
            context_window: None,
        };
        let mut tui = Tui::new(cfg);
        tui.slash("compact");
        assert!(!tui.compacting, "small session must not start compaction");
        assert!(
            tui.entries
                .iter()
                .any(|e| matches!(e, Entry::Notice(n) if n.contains("too small")))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn steering_response_renders_after_tool() {
        let dir = std::env::temp_dir().join(format!("axe-steer-tui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = TuiConfig {
            base: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            system: String::new(),
            dir: String::new(),
            axe_root: dir.to_str().unwrap().to_string(),
            session_dir: dir.to_str().unwrap().to_string(),
            api_key: "k".into(),
            resume: None,
            context_window: None,
        };
        let mut tui = Tui::new(cfg);
        let (tx, rx) = std::sync::mpsc::channel::<TurnEvent>();
        tui.rx = Some(rx);
        let (steer_tx, _steer_rx) = std::sync::mpsc::channel::<String>();
        tui.steer_tx = Some(steer_tx);

        let send = |ev: TurnEvent| {
            tx.send(ev).unwrap();
        };
        send(TurnEvent::AssistantDelta("initial ".into()));
        send(TurnEvent::AssistantDelta("answer".into()));
        send(TurnEvent::AssistantDone);
        send(TurnEvent::ToolStart {
            label: "Running sleep".into(),
            kind: "command".into(),
        });
        send(TurnEvent::ToolDelta("partial".into()));
        assert!(tui.drain_events());

        tui.input.buf = "continue".into();
        tui.steer();
        assert!(
            tui.entries
                .iter()
                .any(|e| matches!(e, Entry::User(u) if u == "continue"))
        );

        send(TurnEvent::ToolResult {
            label: "Ran sleep".into(),
            kind: "command".into(),
        });
        send(TurnEvent::AssistantDelta("steered ".into()));
        send(TurnEvent::AssistantDelta("reply".into()));
        send(TurnEvent::AssistantDone);
        assert!(tui.drain_events());

        let user_idx = tui
            .entries
            .iter()
            .position(|e| matches!(e, Entry::User(u) if u == "continue"))
            .expect("steer user entry");
        let after: Vec<&String> = tui.entries[user_idx + 1..]
            .iter()
            .filter_map(|e| match e {
                Entry::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(
            after.iter().any(|t| t.contains("steered reply")),
            "response not rendered after steer: {after:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worker_steer_events_reach_transcript() {
        use crate::run::{self, RunOptions};
        use std::cell::RefCell;
        use std::collections::VecDeque;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;

        struct SeqProvider {
            responses: RefCell<VecDeque<crate::Response>>,
        }
        impl crate::Provider for SeqProvider {
            fn complete(&self, _req: &crate::Request) -> Result<crate::Response, crate::Error> {
                Ok(self
                    .responses
                    .borrow_mut()
                    .pop_front()
                    .expect("no fake response"))
            }
            fn stream(
                &self,
                _req: &crate::Request,
                _cancel: &Arc<AtomicBool>,
            ) -> crate::StreamHandle {
                let (tx, rx) = mpsc::channel();
                let resp = self.complete(_req).expect("no fake response");
                let thread = std::thread::spawn(move || {
                    if !resp.message.content.is_empty() {
                        let _ = tx.send(crate::StreamEvent::Content(resp.message.content.clone()));
                    }
                    for c in &resp.message.tool_calls {
                        let _ = tx.send(crate::StreamEvent::ToolCall(c.clone()));
                    }
                    let _ = tx.send(crate::StreamEvent::Done);
                    Ok(resp)
                });
                crate::StreamHandle::new(rx, thread)
            }
        }

        let dir = std::env::temp_dir().join(format!("axe-worker-steer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = TuiConfig {
            base: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            system: String::new(),
            dir: String::new(),
            axe_root: dir.to_str().unwrap().to_string(),
            session_dir: dir.to_str().unwrap().to_string(),
            api_key: "k".into(),
            resume: None,
            context_window: None,
        };
        let mut tui = Tui::new(cfg);
        tui.entries.clear();
        tui.entries.push(Entry::User("go".into()));
        tui.msgs = vec![crate::Message {
            role: "user".into(),
            content: "go".into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
        }];

        let bash_tool = crate::tools::bash("");
        let p = SeqProvider {
            responses: RefCell::new(VecDeque::from([
                crate::Response {
                    message: crate::Message {
                        role: "assistant".into(),
                        content: String::new(),
                        tool_calls: vec![crate::ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: r#"{"command":"sleep 0.2"}"#.into(),
                        }],
                        tool_call_id: String::new(),
                    },
                    usage: crate::Usage::default(),
                    stop_reason: String::new(),
                },
                crate::Response {
                    message: crate::Message {
                        role: "assistant".into(),
                        content: "steered answer".into(),
                        tool_calls: Vec::new(),
                        tool_call_id: String::new(),
                    },
                    usage: crate::Usage::default(),
                    stop_reason: String::new(),
                },
            ])),
        };

        let (tx, rx) = mpsc::channel::<TurnEvent>();
        let (steer_tx, steer_rx) = mpsc::channel::<String>();
        tui.steer_tx = Some(steer_tx.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let msgs = tui.msgs.clone();
        let worker = std::thread::spawn(move || {
            let opts = RunOptions {
                model: "m",
                system: "",
                tools: &[bash_tool],
                max_turns: 5,
            };
            let mut sink = TuiSink {
                tx: &tx,
                steer: steer_rx,
                threshold: None,
            };
            let end = run::run_stream(&p, &opts, &msgs, &cancel, &mut sink);
            let (err, cancelled, compact) = match end.outcome {
                run::Outcome::Done => (None, false, false),
                run::Outcome::MaxTurns => (Some("stopped: max turns reached".into()), false, false),
                run::Outcome::Cancelled => (None, true, false),
                run::Outcome::Compact => (None, false, true),
                run::Outcome::Failed(e) => (Some(e), false, false),
            };
            let _ = tx.send(TurnEvent::End {
                messages: end.messages,
                usage: end.usage,
                err,
                cancelled,
                compact,
            });
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        tui.input.buf = "continue".into();
        tui.steer();
        let _ = worker.join();

        tui.rx = Some(rx);
        tui.drain_events();
        assert!(
            tui.entries
                .iter()
                .any(|e| matches!(e, Entry::User(u) if u == "continue")),
            "steer user entry missing"
        );
        let texts: Vec<String> = tui
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("steered answer")),
            "steered response missing from transcript: {texts:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
