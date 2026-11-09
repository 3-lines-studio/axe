use crate::app;
use crate::session::{self, SessionState, Store};
use crate::{Image, Message};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

mod controller;
mod render;
mod runtime;
mod state;
mod status;
mod theme;

use render::*;
use runtime::TurnEvent;
use state::{Entry, Picker};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const ARM_WINDOW: Duration = Duration::from_millis(800);

pub struct TuiConfig {
    pub base: String,
    pub model: String,
    pub spec: crate::ModelSpec,
    pub system: String,
    pub dir: String,
    pub store: Arc<dyn Store>,
    pub api_key: String,
    pub resume: Option<app::Resume>,
}

#[derive(Default)]
struct DoublePress {
    at: Option<Instant>,
}

impl DoublePress {
    fn armed(&self) -> bool {
        self.at.is_some_and(|at| at.elapsed() < ARM_WINDOW)
    }

    fn arm(&mut self) {
        self.at = Some(Instant::now());
    }

    fn clear(&mut self) {
        self.at = None;
    }
}

enum Turn {
    Idle,
    Running {
        events: Receiver<TurnEvent>,
        steer: Sender<String>,
        base: usize,
    },
    Compacting {
        events: Receiver<TurnEvent>,
        resume: bool,
    },
}

impl Turn {
    fn busy(&self) -> bool {
        !matches!(self, Turn::Idle)
    }

    fn compacting(&self) -> bool {
        matches!(self, Turn::Compacting { .. })
    }
}

enum ToolActivity {
    Idle,
    Running {
        label: String,
        output: Option<String>,
    },
}

enum Scroll {
    Bottom,
    At(u16),
}

impl Scroll {
    fn row(&self, max: u16) -> u16 {
        match self {
            Scroll::Bottom => max,
            Scroll::At(row) => (*row).min(max),
        }
    }
}

#[derive(Clone, Default)]
struct Catalog {
    query: String,
    selected: usize,
}

enum CatalogKey {
    Close,
    Select,
    Keep,
}

impl Catalog {
    fn key(&mut self, key: KeyEvent, count: usize) -> CatalogKey {
        match key.code {
            KeyCode::Esc => return CatalogKey::Close,
            KeyCode::Enter => return CatalogKey::Select,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            _ => {
                if self.edit(key) {
                    self.selected = 0;
                }
            }
        }
        CatalogKey::Keep
    }

    fn edit(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Char(character) => {
                self.query.push(character);
                true
            }
            KeyCode::Backspace => {
                self.query.pop();
                true
            }
            _ => false,
        }
    }
}

enum Overlay {
    None,
    Picker(Picker),
    Help(Catalog),
    Resume {
        catalog: Catalog,
        sessions: Vec<session::SessionMeta>,
    },
    Rewind {
        catalog: Catalog,
        items: Vec<app::RewindItem>,
    },
    RewindPending,
}

enum Selection {
    Command(app::Command),
    Session(String),
    Rewind(usize),
}

enum SessionOrigin {
    Live,
    Resumed(String),
}

enum Next {
    Keep,
    Idle,
    Failed,
    Lost,
    Compact { resume: bool },
    Turn,
}

struct TranscriptCache {
    lines: Vec<Line<'static>>,
    width: usize,
    entries: usize,
    dirty: bool,
}

impl Default for TranscriptCache {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            width: 0,
            entries: 0,
            dirty: true,
        }
    }
}

impl TranscriptCache {
    fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn text(&mut self, entries: &[Entry], width: usize) -> Text<'static> {
        let live = matches!(entries.last(), Some(Entry::Assistant { .. }));
        let prefix = entries.len() - usize::from(live);
        if self.dirty || self.width != width || self.entries > prefix {
            self.lines.clear();
            push_transcript_header(&mut self.lines);
            self.width = width;
            self.entries = 0;
            self.dirty = false;
        }
        while self.entries < prefix {
            push_transcript_entry(&mut self.lines, &entries[self.entries], width);
            self.entries += 1;
        }
        let mut lines = self.lines.clone();
        if live {
            push_transcript_entry(&mut lines, &entries[prefix], width);
        }
        Text::from(lines)
    }
}

struct TerminalRestore {
    enhanced_keyboard: bool,
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        status::clear();
        let _ = disable_raw_mode();
        if self.enhanced_keyboard {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

pub fn run(cfg: TuiConfig) -> Result<(), String> {
    theme::init();
    enable_raw_mode().map_err(|error| error.to_string())?;
    let restore = TerminalRestore {
        enhanced_keyboard: true,
    };
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .map_err(|error| error.to_string())?;
    // Some terminals (Herdr) reset the Kitty keyboard flags when switching to
    // the alternate screen, so request them only after entering it.
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .map_err(|error| error.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
    status::report(status::State::Idle);
    let result = run_app(&mut terminal, cfg);
    drop(restore);
    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cfg: TuiConfig,
) -> Result<(), String> {
    cfg.store.archive();
    let skills = crate::skills::discover(std::path::Path::new(&cfg.dir));
    let mut app = App {
        cfg,
        skills,
        entries: Vec::new(),
        messages: Vec::new(),
        input: String::new(),
        cursor: 0,
        attachments: Vec::new(),
        scroll: Scroll::Bottom,
        max_scroll: 0,
        page_size: 1,
        turn: Turn::Idle,
        tool: ToolActivity::Idle,
        cancel: Arc::new(AtomicBool::new(false)),
        overflow_retried: false,
        input_tokens: 0,
        output_tokens: 0,
        session_input: 0,
        session_output: 0,
        session_state: SessionState::default(),
        turn_started: Instant::now(),
        spinner_frame: 0,
        width: 80,
        history: Vec::new(),
        history_index: None,
        want_quit: false,
        ctrl_c: DoublePress::default(),
        esc: DoublePress::default(),
        overlay: Overlay::None,
        origin: SessionOrigin::Live,
        markdown_dirty: false,
        transcript: TranscriptCache::default(),
    };
    match app.cfg.resume.clone() {
        Some(app::Resume::Picker) => app.open_resume(),
        Some(app::Resume::Session(id)) => app.resume(&id),
        None => {}
    }
    let mut dirty = true;
    loop {
        if app.drain_events() {
            dirty = true;
        }
        if app.want_quit {
            app.on_exit();
            return Ok(());
        }
        if app.turn.busy() {
            let frame = spinner_frame(app.turn_started.elapsed());
            if frame != app.spinner_frame {
                app.spinner_frame = frame;
                dirty = true;
            }
        }
        if dirty {
            terminal
                .draw(|frame| app.draw(frame))
                .map_err(|error| error.to_string())?;
            dirty = false;
        }
        let wait = if app.turn.busy() {
            Duration::from_millis(30)
        } else {
            Duration::from_millis(250)
        };
        if !event::poll(wait).map_err(|error| error.to_string())? {
            continue;
        }
        dirty = true;
        match event::read().map_err(|error| error.to_string())? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if app.handle_key(key) {
                    app.on_exit();
                    return Ok(());
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(3),
                MouseEventKind::ScrollDown => app.scroll_down(3),
                _ => {}
            },
            Event::Paste(text) => app.paste(&text),
            _ => {}
        }
    }
}

struct App {
    cfg: TuiConfig,
    entries: Vec<Entry>,
    messages: Vec<Message>,
    input: String,
    cursor: usize,
    attachments: Vec<Image>,
    scroll: Scroll,
    max_scroll: u16,
    page_size: u16,
    turn: Turn,
    tool: ToolActivity,
    cancel: Arc<AtomicBool>,
    overflow_retried: bool,
    input_tokens: usize,
    output_tokens: usize,
    session_input: usize,
    session_output: usize,
    session_state: SessionState,
    turn_started: Instant,
    spinner_frame: usize,
    width: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    want_quit: bool,
    ctrl_c: DoublePress,
    esc: DoublePress,
    overlay: Overlay,
    skills: Vec<crate::skills::Skill>,
    origin: SessionOrigin,
    markdown_dirty: bool,
    transcript: TranscriptCache,
}

fn push_transcript_header(lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(vec![
        Span::styled(
            "axe",
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::styled(
            format!(" v{VERSION} · Run /help for commands"),
            Style::default(),
        ),
    ]));
    lines.push(Line::default());
}

const TEXT_PADDING: usize = 2;

fn push_transcript_entry(lines: &mut Vec<Line<'static>>, entry: &Entry, width: usize) {
    let text_width = width.saturating_sub(TEXT_PADDING);
    match entry {
        Entry::User { text, images } => {
            for label in images {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default()),
                    Span::styled(
                        format!("[image {label}]"),
                        Style::default().fg(theme::palette().muted),
                    ),
                ]));
            }
            for line in text.lines() {
                for chunk in wrap_input_line(line, width.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::styled("┃ ", Style::default()),
                        Span::styled(chunk, Style::default().add_modifier(Modifier::BOLD)),
                    ]));
                }
            }
        }
        Entry::Assistant { rendered, .. } => {
            for line in rendered {
                lines.extend(wrap_markdown_line(line, width));
            }
        }
        Entry::Tool(calls) => {
            let style = Style::default().fg(theme::palette().muted);
            if calls.len() == 1 {
                lines.push(Line::from(vec![
                    Span::styled("● ", style),
                    Span::styled(truncate(&calls[0], text_width.saturating_sub(2)), style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("● ", style),
                    Span::styled(format!("{} tool calls", calls.len()), style),
                ]));
                for (index, call) in calls.iter().enumerate() {
                    let branch = if index + 1 == calls.len() {
                        "└"
                    } else {
                        "├"
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{branch} {}", truncate(call, text_width.saturating_sub(2))),
                        style,
                    )));
                }
            }
        }
        Entry::Notice(text) => lines.push(Line::from(Span::styled(text.clone(), Style::default()))),
    }
    lines.push(Line::default());
}

fn filtered_sessions<'a>(
    sessions: &'a [session::SessionMeta],
    query: &str,
) -> Vec<&'a session::SessionMeta> {
    let query = query.to_lowercase();
    sessions
        .iter()
        .filter(|session| {
            query.is_empty()
                || session.title.to_lowercase().contains(&query)
                || session.id.to_lowercase().contains(&query)
        })
        .collect()
}

impl App {
    fn draw(&mut self, frame: &mut ratatui::Frame) {
        match &self.overlay {
            Overlay::Help(catalog) => return self.draw_help(frame, catalog),
            Overlay::Resume { catalog, sessions } => {
                return self.draw_resume(frame, catalog, sessions);
            }
            Overlay::Rewind { catalog, items } => {
                return self.draw_rewind(frame, catalog, items);
            }
            _ => {}
        }
        self.sync_picker();
        let width = frame.area().width.max(1);
        let input_width = width.saturating_sub(2).max(1) as usize;
        let input_lines = self
            .input
            .split('\n')
            .map(|line| wrap_input_line(line, input_width).len())
            .sum::<usize>();
        let composer_lines = input_lines + self.attachments.len();
        let input_height =
            composer_lines.clamp(1, frame.area().height.saturating_div(2) as usize) as u16;
        let (cursor_row, cursor_col) = self.cursor_position(input_width);
        let cursor_row =
            (self.attachments.len() + cursor_row).min(composer_lines.saturating_sub(1));
        let visible_start = input_scroll(cursor_row, composer_lines, input_height as usize);
        let activity_height = match (&self.tool, self.turn.busy()) {
            (
                ToolActivity::Running {
                    output: Some(_), ..
                },
                _,
            ) => 2,
            _ => u16::from(self.turn.busy()),
        };
        let picker_height = match &self.overlay {
            Overlay::Picker(picker) => picker.items.len().min(6) as u16 + 2,
            _ => 0,
        };
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(activity_height),
                Constraint::Length(input_height),
                Constraint::Length(picker_height),
                Constraint::Length(1),
            ])
            .split(frame.area());
        let transcript_width = areas[0].width.max(1) as usize;
        self.width = transcript_width;
        self.sync_markdown_width(transcript_width);
        let transcript = self.transcript.text(&self.entries, transcript_width);
        let transcript = Paragraph::new(transcript).wrap(Wrap { trim: false });
        let line_count = transcript.line_count(areas[0].width.max(1)) as u16;
        self.page_size = areas[0].height.max(1);
        self.max_scroll = line_count.saturating_sub(self.page_size);
        let transcript = transcript.scroll((self.scroll.row(self.max_scroll), 0));
        frame.render_widget(transcript, areas[0]);
        if self.turn.busy() {
            let palette = theme::palette();
            let elapsed = self.turn_started.elapsed();
            let (label, color) = match &self.tool {
                ToolActivity::Running { label, .. } => (label.clone(), palette.tool),
                ToolActivity::Idle if self.turn.compacting() => {
                    ("Compacting".to_string(), palette.secondary)
                }
                ToolActivity::Idle => (
                    format!(
                        "Thinking ({}s) (↑{} ↓{})",
                        elapsed.as_secs(),
                        format_tokens(self.input_tokens),
                        format_tokens(self.output_tokens)
                    ),
                    palette.strong,
                ),
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    spinner_glyph(spinner_frame(elapsed)),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    truncate(&label, usize::from(width).saturating_sub(2 + TEXT_PADDING)),
                    Style::default(),
                ),
            ])];
            if let ToolActivity::Running {
                output: Some(output),
                ..
            } = &self.tool
            {
                lines.push(Line::from(Span::styled(
                    format!("  {}", output.lines().last().unwrap_or_default()),
                    Style::default(),
                )));
            }
            frame.render_widget(Paragraph::new(lines).style(Style::default()), areas[1]);
        }
        let input = Paragraph::new(self.input_text(input_width)).scroll((visible_start as u16, 0));
        frame.render_widget(input, areas[2]);
        if let Overlay::Picker(picker) = &self.overlay {
            let mut lines = vec![Line::from(Span::styled(
                "─".repeat(width as usize),
                Style::default(),
            ))];
            let start = picker.selected.saturating_sub(5);
            for (index, item) in picker.items.iter().enumerate().skip(start).take(6) {
                let style = if index == picker.selected {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(format!("  {item}"), style)));
            }
            lines.push(Line::from(Span::styled(
                "─".repeat(width as usize),
                Style::default(),
            )));
            frame.render_widget(Paragraph::new(lines), areas[3]);
        }
        let mut status = if self.ctrl_c.armed() {
            "press ctrl+c again to exit".to_string()
        } else {
            let mut parts = vec![self.cfg.model.clone()];
            if let Some(window) = self.cfg.spec.context_window
                && window > 0
            {
                let percent = self.input_tokens.saturating_mul(100) / window;
                if width <= 60 {
                    parts.push(format!("{percent}%"));
                } else {
                    parts.push(format!(
                        "{}/{} ({percent}%)",
                        format_tokens(self.input_tokens),
                        format_tokens(window)
                    ));
                }
            }
            if width > 60 {
                parts.push(format!(
                    "↑{} ↓{}",
                    format_tokens(self.session_input),
                    format_tokens(self.session_output)
                ));
            }
            parts.join(" · ")
        };
        if !matches!(self.scroll, Scroll::Bottom) {
            status.push_str(&format!(
                " · {}/{}",
                self.scroll.row(self.max_scroll),
                self.max_scroll
            ));
        }
        frame.render_widget(Clear, areas[4]);
        frame.render_widget(Paragraph::new(status).style(Style::default()), areas[4]);
        frame.set_cursor_position((
            areas[2].x + 2 + cursor_col as u16,
            areas[2].y + (cursor_row - visible_start) as u16,
        ));
    }

    fn draw_help(&self, frame: &mut ratatui::Frame, catalog: &Catalog) {
        let commands = app::command_search(&catalog.query);
        let area = frame.area();
        let visible = area.height.saturating_sub(4) as usize;
        let start = catalog.selected.saturating_sub(visible.saturating_sub(1));
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Help {}", commands.len()),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )),
            Line::default(),
        ];
        for (index, command) in commands.iter().enumerate().skip(start).take(visible) {
            let style = if index == catalog.selected {
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
            };
            let row = if area.width <= 60 {
                format!("  {:<12} {}", command.help, command.description)
            } else {
                format!(
                    "  {:<12} {:<44} {}",
                    command.help, command.description, command.category
                )
            };
            lines.push(Line::from(Span::styled(row, style)));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "Search: {}     ↑↓ Navigate     Enter Open     Esc Close",
                catalog.query
            ),
            Style::default(),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_resume(
        &self,
        frame: &mut ratatui::Frame,
        catalog: &Catalog,
        sessions: &[session::SessionMeta],
    ) {
        let sessions = filtered_sessions(sessions, &catalog.query);
        let area = frame.area();
        let visible = area.height.saturating_sub(4) as usize;
        let start = catalog.selected.saturating_sub(visible.saturating_sub(1));
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Resume {}", sessions.len()),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )),
            Line::default(),
        ];
        for (index, session) in sessions.iter().enumerate().skip(start).take(visible) {
            let style = if index == catalog.selected {
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
            };
            let row = if area.width <= 60 {
                format!("  {}", session.title)
            } else {
                format!(
                    "  {:<44} {} turns · {}",
                    session.title,
                    session.turns,
                    age(session.updated)
                )
            };
            lines.push(Line::from(Span::styled(row, style)));
        }
        if sessions.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No saved sessions",
                Style::default(),
            )));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "Search: {}     ↑↓ Navigate     Enter Resume     Esc Close",
                catalog.query
            ),
            Style::default(),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn open_rewind(&mut self) {
        if self.turn.busy() {
            self.overlay = Overlay::RewindPending;
            return;
        }
        let items = app::rewind_items(&self.messages);
        self.overlay = Overlay::Rewind {
            catalog: Catalog {
                query: String::new(),
                selected: items.len().saturating_sub(1),
            },
            items,
        };
    }

    fn draw_rewind(
        &self,
        frame: &mut ratatui::Frame,
        catalog: &Catalog,
        items: &[app::RewindItem],
    ) {
        let items = app::filter_rewind_items(items, &catalog.query);
        let area = frame.area();
        let visible = area.height.saturating_sub(4) as usize;
        let start = catalog.selected.saturating_sub(visible.saturating_sub(1));
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Rewind {}", items.len()),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )),
            Line::default(),
        ];
        for (index, item) in items.iter().enumerate().skip(start).take(visible) {
            let style = if index == catalog.selected {
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
            };
            let role = if item.role == "user" {
                "you"
            } else {
                "assistant"
            };
            lines.push(Line::from(Span::styled(
                format!("  {:<52} {role}", item.preview),
                style,
            )));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "Search: {}     ↑↓ Navigate     Enter Rewind     Esc Close",
                catalog.query
            ),
            Style::default(),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn rebuild_transcript(&mut self) {
        let messages = self.messages.clone();
        self.rebuild_transcript_from(&messages);
    }

    fn rebuild_transcript_from(&mut self, messages: &[Message]) {
        self.transcript.invalidate();
        self.entries.clear();
        for message in messages {
            match message.role.as_str() {
                "user" => self.entries.push(Entry::User {
                    text: crate::skills::activated_name(&message.content)
                        .map(|name| format!("/skill {name}"))
                        .unwrap_or_else(|| message.content.clone()),
                    images: message.images.iter().map(crate::image::label).collect(),
                }),
                "assistant" => {
                    if !message.content.is_empty() {
                        self.entries
                            .push(assistant_entry(message.content.clone(), self.width));
                    }
                    for call in &message.tool_calls {
                        let label = app::tool_label(call, false);
                        if let Some(Entry::Tool(calls)) = self.entries.last_mut() {
                            calls.push(label);
                        } else {
                            self.entries.push(Entry::Tool(vec![label]));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn sync_markdown_width(&mut self, width: usize) {
        let mut resized = false;
        for entry in &mut self.entries {
            if let Entry::Assistant {
                source,
                rendered,
                width: entry_width,
            } = entry
                && *entry_width != width
            {
                *rendered = render_markdown(source, width);
                *entry_width = width;
                resized = true;
            }
        }
        if self.markdown_dirty {
            if !resized
                && let Some(Entry::Assistant {
                    source, rendered, ..
                }) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| matches!(entry, Entry::Assistant { .. }))
            {
                *rendered = render_markdown(source, width);
            }
            self.markdown_dirty = false;
        }
    }

    fn input_text(&self, width: usize) -> Text<'static> {
        let mut lines = Vec::new();
        for image in &self.attachments {
            lines.push(Line::from(vec![
                Span::styled("┃ ", Style::default()),
                Span::styled(
                    format!("[image {}]", crate::image::label(image)),
                    Style::default().fg(theme::palette().muted),
                ),
            ]));
        }
        for line in self.input.split('\n') {
            for chunk in wrap_input_line(line, width) {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default()),
                    Span::raw(chunk),
                ]));
            }
        }
        Text::from(lines)
    }

    fn cursor_position(&self, width: usize) -> (usize, usize) {
        cursor_row_col(&self.input, self.cursor, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    #[test]
    fn markdown_render_is_owned_and_visible() {
        let rendered = {
            let source = "# Heading\n\n**bold**".to_string();
            render_markdown(&source, 80)
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 4));
        Paragraph::new(Text::from(rendered)).render(buffer.area, &mut buffer);
        let screen = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Heading"));
        assert!(screen.contains("bold"));
    }

    #[test]
    fn markdown_wrap_keeps_indentation() {
        let normal = render_markdown("alpha beta gamma", 80);
        let normal = wrap_markdown_line(&normal[0], 12);
        assert_eq!(line_text(&normal[1]), "  beta gamma");

        let list = render_markdown("- alpha beta gamma", 80);
        let list = wrap_markdown_line(&list[0], 12);
        assert_eq!(line_text(&list[1]), "    beta");
    }

    #[test]
    fn markdown_wrap_indents_every_marker_kind() {
        // Ordered ("1. ") and task-list ("- [x] ") markers carry the same
        // continuation indent as the bullet marker.
        let ordered = render_markdown("1. alpha beta gamma", 80);
        let ordered = wrap_markdown_line(&ordered[0], 12);
        assert_eq!(line_text(&ordered[1]), "     beta");

        let task = render_markdown("- [x] alpha beta gamma", 80);
        let task = wrap_markdown_line(&task[0], 16);
        assert_eq!(line_text(&task[1]), "        beta");
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn table_stays_boxed_when_it_fits() {
        let source = "| Name | Value |\n|------|-------|\n| foo  | bar   |";
        let text = render_markdown(source, 40)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(text.iter().any(|line| line.contains('┌')));
        assert!(text.iter().any(|line| line.contains("foo")));
    }

    #[test]
    fn table_stacks_when_too_wide() {
        let source = "| Name | Value |\n|------|-------|\n| foo  | bar   |\n| baz  | qux   |";
        let text = render_markdown(source, 16)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(!text.iter().any(|line| line.contains('┌')));
        assert_eq!(text.iter().filter(|line| line.contains("Name:")).count(), 2);
        assert!(
            text.iter()
                .any(|line| line.contains("Value:") && line.contains("bar"))
        );
        assert!(text.iter().any(|line| line.is_empty()));
    }

    #[test]
    fn table_cells_unescape_pipes() {
        assert_eq!(
            table_cells("| a \\| b | c |"),
            Some(vec!["a | b".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn fenced_code_is_never_rewritten() {
        let fenced = "```\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n";
        let rewritten = mobile_tables(fenced, 10, true);
        assert!(rewritten.contains("| A | B |"), "got: {rewritten}");
        assert!(rewritten.contains("|---|---|"), "got: {rewritten}");
        assert!(rewritten.contains("| 1 | 2 |"), "got: {rewritten}");
        assert!(
            !rewritten.contains("**A:**"),
            "code fence rewritten: {rewritten}"
        );

        // A real table after the fence still stacks.
        let mixed = "```\ncode\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n";
        let out = mobile_tables(mixed, 10, false);
        assert!(out.contains("code"), "got: {out}");
        assert!(out.contains("**A:** 1"), "real table not stacked: {out}");
        assert!(!out.contains("| A | B |"), "real table left boxed: {out}");
    }

    fn box_overflows(source: &str, width: usize) -> bool {
        render_markdown(source, width).iter().any(|line| {
            let text = line_text(line);
            (text.contains('┌') || text.contains('│')) && line.width() > width
        })
    }

    #[test]
    fn tables_never_overflow_at_any_width() {
        let sources = [
            "| Model | Context | Input $/M | Output $/M | Notes |\n|-------|--------:|----------:|-----------:|-------|\n| gpt-4o | 128000 | 2.50 | 10.00 | multimodal |\n| claude-3-5-sonnet | 200000 | 3.00 | 15.00 | long context window |",
            "- models:\n\n  | Model | Context | Input $/M | Output $/M | Notes |\n  |-------|--------:|----------:|-----------:|-------|\n  | gpt-4o | 128000 | 2.50 | 10.00 | multimodal |\n  | claude-3-5-sonnet | 200000 | 3.00 | 15.00 | long context window |",
            "> | Model | Context | Input $/M | Output $/M | Notes |\n> |-------|--------:|----------:|-----------:|-------|\n> | gpt-4o | 128000 | 2.50 | 10.00 | multimodal |\n> | claude-3-5-sonnet | 200000 | 3.00 | 15.00 | long context window |",
            "> | Key | Description |\n> |-----|-------------|\n> | id | a very long description that keeps going and going |",
            "> > | A | B |\n> > |---|---|\n> > | 1 | 2 |",
            "| 名前 | 説明 |\n|------|------|\n| 識別子 | とても長い説明がここに続きます |",
            "> | 名前 | 説明 |\n> |------|------|\n> | 識別子 | とても長い説明がここに続きます |",
        ];
        for source in sources {
            for width in 6..160 {
                assert!(
                    !box_overflows(source, width),
                    "table overflows at width {width}: {source:?}"
                );
            }
        }
    }

    #[test]
    fn blockquote_wrap_keeps_marker() {
        let source = "> | Key | Description |\n> |-----|-------------|\n> | id | a very long description that keeps going and going |";
        let wrapped = render_markdown(source, 24)
            .iter()
            .flat_map(|line| wrap_markdown_line(line, 24))
            .map(|line| line_text(&line))
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert!(wrapped.len() > 2);
        assert!(
            wrapped
                .iter()
                .all(|line| line.trim_start().starts_with('>')),
            "wrapped blockquote lost its marker: {wrapped:?}"
        );
    }

    #[test]
    fn scroll_to_bottom_reaches_last_line() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let width = 16u16;
        let height = 3u16;
        let paragraph = Paragraph::new(text.to_string()).wrap(Wrap { trim: false });
        let rows = paragraph.line_count(width);
        let max_scroll = rows.saturating_sub(height as usize) as u16;
        assert!(max_scroll > 0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        paragraph.render(buffer.area, &mut buffer);
        let screen = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("alpha"));
        assert!(!screen.contains("kappa"));
        let paragraph = Paragraph::new(text.to_string())
            .wrap(Wrap { trim: false })
            .scroll((max_scroll, 0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        paragraph.render(buffer.area, &mut buffer);
        let screen = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("kappa"));
        assert!(!screen.contains("alpha"));
    }

    #[test]
    fn wide_characters_wrap_by_display_width() {
        // Each CJK character occupies two columns, so a 10-column input holds
        // five of them, not ten.
        assert_eq!(
            wrap_input_line("这是一段很长的中文文本", 10),
            vec![
                "这是一段很".to_string(),
                "长的中文文".to_string(),
                "本".to_string()
            ]
        );
        assert_eq!(columns("这是"), 4);
        assert_eq!(take_columns("这是一段", 5), "这是");
        assert_eq!(truncate("这是一段很长的中文文本", 6), "这是…");

        let source = "- 项目 alpha beta gamma delta epsilon zeta eta theta";
        for width in 8..60usize {
            let rendered = render_markdown(source, width);
            for line in &rendered {
                for wrapped in wrap_markdown_line(line, width) {
                    assert!(
                        wrapped.width() <= width,
                        "w={width} got {}: {:?}",
                        wrapped.width(),
                        line_text(&wrapped)
                    );
                }
            }
        }
    }

    #[test]
    fn cursor_follows_display_width() {
        assert_eq!(wrap_input_line("这是一段", 5), vec!["这是", "一段"]);
        assert_eq!(cursor_row_col("这是一段", 0, 5), (0, 0));
        assert_eq!(cursor_row_col("这是一段", "这是".len(), 5), (1, 0));
        assert_eq!(cursor_row_col("abcdef", 3, 5), (0, 3));
        assert_eq!(cursor_row_col("abcdef", 5, 5), (1, 0));
        assert_eq!(cursor_row_col("ab\ncd", 4, 5), (1, 1));
    }

    #[test]
    fn input_scroll_keeps_the_cursor_visible() {
        assert_eq!(input_scroll(0, 4, 10), 0);
        assert_eq!(input_scroll(9, 10, 10), 0);
        assert_eq!(input_scroll(0, 100, 10), 0);
        assert_eq!(input_scroll(99, 100, 10), 90);
        assert_eq!(input_scroll(42, 100, 10), 33);
        assert_eq!(input_scroll(100, 100, 10), 90);
    }

    #[test]
    fn truncate_shortens_with_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abcdefghij", 0), "…");
    }

    #[test]
    fn tool_lines_fill_the_width_but_keep_padding() {
        let call = format!("Ran {}", "x".repeat(200));
        let mut lines = Vec::new();
        push_transcript_entry(&mut lines, &Entry::Tool(vec![call]), 40);
        assert_eq!(lines[0].width(), 40 - TEXT_PADDING);
        assert!(line_text(&lines[0]).ends_with('…'));

        let mut lines = Vec::new();
        push_transcript_entry(&mut lines, &Entry::Tool(vec!["Ran ls".into()]), 40);
        assert_eq!(line_text(&lines[0]), "● Ran ls");
    }

    #[test]
    fn catalog_keeps_its_selection_inside_the_list() {
        let mut catalog = Catalog::default();
        assert!(matches!(
            catalog.key(KeyEvent::from(KeyCode::Up), 3),
            CatalogKey::Keep
        ));
        assert_eq!(catalog.selected, 0);
        for _ in 0..5 {
            catalog.key(KeyEvent::from(KeyCode::Down), 3);
        }
        assert_eq!(catalog.selected, 2);
        catalog.key(KeyEvent::from(KeyCode::Char('a')), 3);
        assert_eq!(catalog.query, "a");
        assert_eq!(catalog.selected, 0);
        assert!(matches!(
            catalog.key(KeyEvent::from(KeyCode::Esc), 3),
            CatalogKey::Close
        ));
    }

    #[test]
    fn scroll_past_the_end_follows_the_bottom_again() {
        assert_eq!(Scroll::Bottom.row(10), 10);
        assert_eq!(Scroll::At(4).row(10), 4);
        assert_eq!(Scroll::At(40).row(10), 10);
    }

    #[test]
    fn format_tokens_shortens_large_values() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(9_999), "10.0k");
        assert_eq!(format_tokens(10_000), "10k");
    }

    #[test]
    fn byte_column_handles_unicode() {
        assert_eq!(byte_at_column("a—c", 0, 5, 2), 4);
    }

    #[test]
    fn input_wrap_splits_on_width() {
        assert_eq!(wrap_input_line("", 5), vec![""]);
        assert_eq!(wrap_input_line("ab", 5), vec!["ab"]);
        assert_eq!(wrap_input_line("abcde", 5), vec!["abcde"]);
        assert_eq!(wrap_input_line("abcdef", 5), vec!["abcde", "f"]);
        assert_eq!(wrap_input_line("abcdefghij", 5), vec!["abcde", "fghij"]);
    }
}
