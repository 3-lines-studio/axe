use crate::app;
use crate::session::{self, SessionState, Store};
use crate::{Image, Message};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph};
use std::io::{self, Stdout, Write};
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
use state::{Entry, Picker, ToolItem};

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
    Running { label: app::ToolLabel },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    line: usize,
    column: usize,
}

#[derive(Clone, Copy)]
struct TextSelection {
    anchor: Point,
    focus: Point,
}

impl TextSelection {
    fn ordered(&self) -> (Point, Point) {
        let anchor = (self.anchor.line, self.anchor.column);
        let focus = (self.focus.line, self.focus.column);
        if focus < anchor {
            (self.focus, self.anchor)
        } else {
            (self.anchor, self.focus)
        }
    }

    fn cells(&self, line: usize) -> Option<(usize, usize)> {
        let (start, end) = self.ordered();
        if line < start.line || line > end.line {
            return None;
        }
        let first = if line == start.line { start.column } else { 0 };
        let last = if line == end.line {
            end.column
        } else {
            usize::MAX
        };
        Some((first, last))
    }
}

/// Selection point under a screen cell, in transcript lines rather than rows.
fn point_at(area: Rect, scroll: u16, column: u16, row: u16) -> Point {
    let row = row
        .saturating_sub(area.y)
        .min(area.height.saturating_sub(1));
    let column = column
        .saturating_sub(area.x)
        .min(area.width.saturating_sub(1));
    Point {
        line: usize::from(scroll) + usize::from(row),
        column: usize::from(column),
    }
}

fn copy_to_clipboard(text: &str) {
    let encoded = crate::image::base64(text.as_bytes());
    print!("\u{1b}]52;c;{encoded}\u{1b}\\");
    let _ = io::stdout().flush();
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
            push_transcript_header(&mut self.lines, width);
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
        transcript_area: Rect::default(),
        selection: None,
        pending_copy: false,
        flash: None,
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
        match event::read().map_err(|error| error.to_string())? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                dirty = true;
                if app.handle_key(key) {
                    app.on_exit();
                    return Ok(());
                }
            }
            Event::Mouse(mouse) => dirty |= app.handle_mouse(mouse),
            Event::Paste(text) => {
                app.paste(&text);
                dirty = true;
            }
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
    transcript_area: Rect,
    selection: Option<TextSelection>,
    pending_copy: bool,
    flash: Option<String>,
}

fn push_transcript_header(lines: &mut Vec<Line<'static>>, width: usize) {
    let badge = take_columns("axe", width);
    let rest = take_columns(
        &format!(" v{VERSION} · Run /help for commands"),
        width.saturating_sub(columns(&badge)),
    );
    lines.push(Line::from(vec![
        Span::styled(
            badge,
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::styled(rest, Style::default()),
    ]));
    lines.push(Line::default());
}

const TEXT_PADDING: usize = 2;
const SLOW_TOOL: Duration = Duration::from_millis(100);

fn tool_prefix(branch: &str, failed: bool) -> String {
    match (branch, failed) {
        (branch, false) => branch.to_string(),
        ("● ", true) => "✗ ".to_string(),
        (branch, true) => format!("{branch}✗ "),
    }
}

fn tool_line(branch: &str, item: &ToolItem, text_width: usize) -> Line<'static> {
    let failed = item.outcome.is_some_and(|outcome| outcome.error);
    let style = Style::default().fg(if failed {
        theme::palette().tool
    } else {
        theme::palette().muted
    });
    let prefix = tool_prefix(branch, failed);
    let suffix = match item.outcome {
        Some(outcome) if outcome.elapsed >= SLOW_TOOL => {
            format!(" · {}", elapsed(outcome.elapsed))
        }
        _ => String::new(),
    };
    let room = text_width.saturating_sub(columns(&prefix) + columns(&suffix));
    let body = fit_label(&item.label, room);
    Line::from(Span::styled(format!("{prefix}{body}{suffix}"), style))
}

fn push_transcript_entry(lines: &mut Vec<Line<'static>>, entry: &Entry, width: usize) {
    let text_width = width.saturating_sub(TEXT_PADDING);
    match entry {
        Entry::User { text, images } => {
            for label in images {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default()),
                    Span::styled(
                        truncate(&format!("[image {label}]"), text_width.saturating_sub(2)),
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
        Entry::Tool(items) => {
            let style = Style::default().fg(theme::palette().muted);
            if items.len() == 1 {
                lines.push(tool_line("● ", &items[0], text_width));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("● ", style),
                    Span::styled(
                        truncate(
                            &format!("{} tool calls", items.len()),
                            text_width.saturating_sub(2),
                        ),
                        style,
                    ),
                ]));
                for (index, item) in items.iter().enumerate() {
                    let branch = if index + 1 == items.len() {
                        "└ "
                    } else {
                        "├ "
                    };
                    lines.push(tool_line(branch, item, text_width));
                }
            }
        }
        Entry::Notice(text) => {
            for chunk in wrap_input_line(text, width) {
                lines.push(Line::from(Span::styled(chunk, Style::default())));
            }
        }
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
        let activity_height = u16::from(self.turn.busy());
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
        self.transcript_area = areas[0];
        self.sync_markdown_width(transcript_width);
        let mut transcript = self.transcript.text(&self.entries, transcript_width);
        if self.pending_copy {
            self.pending_copy = false;
            self.copy_selection(&transcript);
        }
        if let Some(selection) = self.selection {
            for (index, line) in transcript.lines.iter_mut().enumerate() {
                if let Some((first, last)) = selection.cells(index) {
                    *line = select(line, first, last);
                }
            }
        }
        let rows = transcript.lines.len().min(usize::from(u16::MAX));
        self.page_size = areas[0].height.max(1);
        self.max_scroll = rows.saturating_sub(usize::from(self.page_size)) as u16;
        let transcript = Paragraph::new(transcript).scroll((self.scroll.row(self.max_scroll), 0));
        frame.render_widget(transcript, areas[0]);
        if self.turn.busy() {
            let palette = theme::palette();
            let elapsed = self.turn_started.elapsed();
            let room = usize::from(width).saturating_sub(2 + TEXT_PADDING);
            let (label, color) = match &self.tool {
                ToolActivity::Running { label } => (fit_label(label, room), palette.tool),
                ToolActivity::Idle if self.turn.compacting() => {
                    (truncate("Compacting", room), palette.secondary)
                }
                ToolActivity::Idle => (
                    truncate(
                        &format!(
                            "Thinking ({}s) (↑{} ↓{})",
                            elapsed.as_secs(),
                            format_tokens(self.input_tokens),
                            format_tokens(self.output_tokens)
                        ),
                        room,
                    ),
                    palette.strong,
                ),
            };
            let lines = vec![Line::from(vec![
                Span::styled(
                    spinner_glyph(spinner_frame(elapsed)),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(label, Style::default()),
            ])];
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
        let mut status = if let Some(flash) = self.flash.clone() {
            flash
        } else if self.ctrl_c.armed() {
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
        if self.flash.is_none() && !matches!(self.scroll, Scroll::Bottom) {
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

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(self.overlay, Overlay::None) {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_up(3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll_down(3);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => self.anchor_at(mouse.column, mouse.row),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(mouse.column, mouse.row),
            MouseEventKind::Up(MouseButton::Left) => self.finish_selection(),
            _ => false,
        }
    }

    fn anchor_at(&mut self, column: u16, row: u16) -> bool {
        let area = self.transcript_area;
        if row < area.y || row >= area.bottom() || column < area.x || column >= area.right() {
            return false;
        }
        let point = self.point_at(column, row);
        self.flash = None;
        self.selection = Some(TextSelection {
            anchor: point,
            focus: point,
        });
        true
    }

    fn drag_to(&mut self, column: u16, row: u16) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let focus = self.point_at(column, row);
        if focus == selection.focus {
            return false;
        }
        self.selection = Some(TextSelection { focus, ..selection });
        true
    }

    fn finish_selection(&mut self) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let (start, end) = selection.ordered();
        if start == end {
            self.selection = None;
            return true;
        }
        self.pending_copy = true;
        true
    }

    fn point_at(&self, column: u16, row: u16) -> Point {
        point_at(
            self.transcript_area,
            self.scroll.row(self.max_scroll),
            column,
            row,
        )
    }

    fn copy_selection(&mut self, transcript: &Text<'static>) {
        let Some(selection) = self.selection else {
            return;
        };
        self.selection = None;
        let mut lines: Vec<String> = Vec::new();
        for (index, line) in transcript.lines.iter().enumerate() {
            let Some((first, last)) = selection.cells(index) else {
                continue;
            };
            let text = selected_text(line, first, last);
            let text = strip_marker(text.trim_end());
            if lines.is_empty() && text.is_empty() {
                continue;
            }
            lines.push(text.to_string());
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if lines.is_empty() {
            return;
        }
        copy_to_clipboard(&lines.join("\n"));
        self.flash = Some(match lines.len() {
            1 => "copied 1 line".to_string(),
            count => format!("copied {count} lines"),
        });
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
        self.selection = None;
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
                        let item = ToolItem {
                            label: app::tool_label(call, false),
                            outcome: None,
                        };
                        if let Some(Entry::Tool(items)) = self.entries.last_mut() {
                            items.push(item);
                        } else {
                            self.entries.push(Entry::Tool(vec![item]));
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
    use ratatui::widgets::{Widget, Wrap};

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
    fn truncate_tail_keeps_the_end() {
        assert_eq!(truncate_tail("short", 10), "short");
        assert_eq!(truncate_tail("abcdefghij", 5), "…ghij");
        assert_eq!(truncate_tail("abcdefghij", 0), "…");
    }

    #[test]
    fn fitting_a_label_keeps_its_verb() {
        let label = |text: &str, keep_tail: bool| app::ToolLabel {
            text: text.to_string(),
            keep_tail,
        };
        assert_eq!(fit_label(&label("Ran ls", true), 20), "Ran ls");
        assert_eq!(
            fit_label(&label("Ran aaaaaaaaaaaaaaaaaaaa", true), 20),
            "Ran …aaaaaaaaaaaaaaa"
        );
        assert_eq!(
            fit_label(&label("Reading aaaaaaaaaaaaaaaaaaaa", false), 20),
            "Reading aaaaaaaaaaa…"
        );
        assert_eq!(fit_label(&label("Ran ls", true), 3), "…ls");
    }

    #[test]
    fn elapsed_reads_ms_below_a_second() {
        assert_eq!(elapsed(Duration::from_millis(3)), "3ms");
        assert_eq!(elapsed(Duration::from_millis(999)), "999ms");
        assert_eq!(elapsed(Duration::from_millis(1500)), "1.5s");
    }

    fn tool(text: &str, keep_tail: bool) -> ToolItem {
        ToolItem {
            label: app::ToolLabel {
                text: text.to_string(),
                keep_tail,
            },
            outcome: None,
        }
    }

    fn finished(text: &str, error: bool, millis: u64) -> ToolItem {
        ToolItem {
            outcome: Some(super::state::ToolOutcome {
                error,
                elapsed: Duration::from_millis(millis),
            }),
            ..tool(text, false)
        }
    }

    #[test]
    fn tool_lines_fill_the_width_but_keep_padding() {
        let mut lines = Vec::new();
        let item = tool(&format!("Ran {}", "x".repeat(200)), false);
        push_transcript_entry(&mut lines, &Entry::Tool(vec![item]), 40);
        assert_eq!(lines[0].width(), 40 - TEXT_PADDING);
        assert!(line_text(&lines[0]).ends_with('…'));

        let mut lines = Vec::new();
        push_transcript_entry(&mut lines, &Entry::Tool(vec![tool("Ran ls", false)]), 40);
        assert_eq!(line_text(&lines[0]), "● Ran ls");
    }

    #[test]
    fn failures_are_marked_and_slow_tools_show_their_time() {
        let mut lines = Vec::new();
        push_transcript_entry(
            &mut lines,
            &Entry::Tool(vec![finished("Read nope.md", true, 3)]),
            40,
        );
        assert_eq!(line_text(&lines[0]), "✗ Read nope.md");

        let mut lines = Vec::new();
        push_transcript_entry(
            &mut lines,
            &Entry::Tool(vec![finished("Ran npm test", false, 1500)]),
            40,
        );
        assert_eq!(line_text(&lines[0]), "● Ran npm test · 1.5s");

        let mut lines = Vec::new();
        let items = vec![
            finished("Ran one", false, 2),
            finished("Read b.rs", true, 1),
        ];
        push_transcript_entry(&mut lines, &Entry::Tool(items), 40);
        assert_eq!(line_text(&lines[1]), "├ Ran one");
        assert_eq!(line_text(&lines[2]), "└ ✗ Read b.rs");
    }

    #[test]
    fn bash_lines_keep_the_end_that_matters() {
        let text = format!("Ran {}", "x".repeat(60));
        let mut lines = Vec::new();
        push_transcript_entry(&mut lines, &Entry::Tool(vec![tool(&text, true)]), 40);
        assert_eq!(line_text(&lines[0]), format!("● Ran …{}", "x".repeat(31)));
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

    #[test]
    fn transcript_lines_never_exceed_the_width() {
        let source = "# Title\n\nA paragraph long enough to wrap several times over a narrow column.\n\n```rust\nfn main() { println!(\"a very wide line of code\"); }\n```\n\n| a | b |\n|---|---|\n| one | two |\n\n- a list item that also needs wrapping";
        let entries = vec![
            Entry::User {
                text: source.to_string(),
                images: vec!["shot.png".into()],
            },
            Entry::Tool(vec![
                tool("Ran ls -la", false),
                tool("Ran cargo test", true),
            ]),
            Entry::Notice("a notice that is longer than the narrow column is".into()),
        ];
        for width in [8usize, 12, 20, 40, 80] {
            let mut lines = Vec::new();
            push_transcript_header(&mut lines, width);
            for entry in &entries {
                push_transcript_entry(&mut lines, entry, width);
            }
            lines.extend(
                render_markdown(source, width)
                    .iter()
                    .flat_map(|line| wrap_markdown_line(line, width)),
            );
            for line in &lines {
                assert!(
                    line.width() <= width,
                    "w={width} got {}: {:?}",
                    line.width(),
                    line_text(line)
                );
            }
        }
    }

    #[test]
    fn selection_reverses_and_returns_the_dragged_cells() {
        let line = Line::from(vec![
            Span::raw("● "),
            Span::styled("hello world", Style::default()),
        ]);
        let marked = select(&line, 2, 6);
        assert_eq!(line_text(&marked), "● hello world");
        assert_eq!(
            marked
                .spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "hello"
        );
        assert_eq!(selected_text(&line, 2, 6), "hello");
        assert_eq!(selected_text(&line, 2, usize::MAX), "hello world");
        assert_eq!(selected_text(&line, 20, usize::MAX), "");

        let wide = Line::from(Span::raw(" 这是一段"));
        assert_eq!(selected_text(&wide, 1, 4), "这是");
        assert_eq!(selected_text(&wide, 2, 4), "这是");
        assert_eq!(selected_text(&wide, 2, 5), "这是一");
    }

    #[test]
    fn selection_drops_the_two_column_gutter() {
        assert_eq!(strip_marker("┃ hi"), "hi");
        assert_eq!(strip_marker("● Ran ls"), "Ran ls");
        assert_eq!(strip_marker("├ second"), "second");
        assert_eq!(strip_marker("  code"), "code");
        assert_eq!(strip_marker("    indented"), "  indented");
        assert_eq!(strip_marker("│ gutter"), "│ gutter");
    }

    #[test]
    fn mouse_cells_map_to_virtual_lines() {
        let area = Rect::new(0, 2, 40, 10);
        assert_eq!(point_at(area, 5, 3, 2), Point { line: 5, column: 3 });
        assert_eq!(
            point_at(area, 5, 39, 9),
            Point {
                line: 12,
                column: 39
            }
        );
        assert_eq!(point_at(area, 0, 200, 200).line, 9);
        assert_eq!(point_at(area, 0, 200, 2).column, 39);
    }

    #[test]
    fn cancellation_ignores_a_single_cell() {
        let start = Point { line: 3, column: 4 };
        let selection = TextSelection {
            anchor: start,
            focus: start,
        };
        assert_eq!(selection.ordered(), (start, start));
        assert_eq!(selection.cells(2), None);
        assert_eq!(selection.cells(3), Some((4, 4)));
        assert_eq!(selection.cells(4), None);

        let backward = TextSelection {
            anchor: Point { line: 5, column: 9 },
            focus: Point { line: 1, column: 2 },
        };
        assert_eq!(backward.ordered().0, Point { line: 1, column: 2 });
        assert_eq!(backward.cells(3), Some((0, usize::MAX)));
    }
}
