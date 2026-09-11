use crate::app;
use crate::openai::OpenAI;
use crate::run::{self, Outcome, RunOptions, Sink};
use crate::session;
use crate::{Message, Tool, ToolCall, Usage};
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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct TuiConfig {
    pub base: String,
    pub model: String,
    pub system: String,
    pub dir: String,
    pub session_dir: String,
    pub api_key: String,
    pub resume: Option<String>,
    pub context_window: Option<usize>,
}

pub fn build_tools(dir: &str) -> Vec<Tool> {
    vec![
        crate::tools::read(),
        crate::tools::write(),
        crate::tools::edit(),
        crate::tools::bash(dir),
    ]
}

enum Entry {
    User(String),
    Assistant {
        source: String,
        rendered: Vec<Line<'static>>,
    },
    Tool(Vec<String>),
    Notice(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Commands,
    Files,
}

struct Picker {
    kind: PickerKind,
    start: usize,
    end: usize,
    items: Vec<String>,
    selected: usize,
}

enum TurnEvent {
    AssistantDelta(String),
    ToolStart(String),
    ToolDelta(String),
    ToolResult(String),
    Tokens(Usage),
    Compacted(Result<(String, usize, Vec<Message>, Vec<session::Entry>), String>),
    End {
        messages: Vec<Message>,
        usage: Usage,
        context: Usage,
        error: Option<String>,
        compact: bool,
    },
}

struct App {
    cfg: TuiConfig,
    entries: Vec<Entry>,
    messages: Vec<Message>,
    input: String,
    cursor: usize,
    scroll: u16,
    max_scroll: u16,
    page_size: u16,
    follow: bool,
    running: bool,
    compacting: bool,
    cancel: Arc<AtomicBool>,
    events: Option<Receiver<TurnEvent>>,
    steer: Option<Sender<String>>,
    retry_after_compact: bool,
    overflow_retried: bool,
    input_tokens: usize,
    output_tokens: usize,
    session_input: usize,
    session_output: usize,
    turn_started: Instant,
    tool_running: Option<String>,
    tool_live: Option<String>,
    history: Vec<String>,
    history_index: Option<usize>,
    want_quit: bool,
    ctrl_c_armed: Option<Instant>,
    picker: Option<Picker>,
    help_open: bool,
    help_selected: usize,
    resume_open: bool,
    resume_selected: usize,
    sessions: Vec<session::SessionMeta>,
    catalog_query: String,
    rewind_open: bool,
    rewind_selected: usize,
    rewind_items: Vec<app::RewindItem>,
    esc_armed: Option<Instant>,
    resume_id: Option<String>,
}

struct TerminalRestore {
    enhanced_keyboard: bool,
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
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
    enable_raw_mode().map_err(|error| error.to_string())?;
    let enhanced_keyboard = matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    let restore = TerminalRestore { enhanced_keyboard };
    let mut stdout = io::stdout();
    if enhanced_keyboard {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            )
        )
        .map_err(|error| error.to_string())?;
    }
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .map_err(|error| error.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
    let result = run_app(&mut terminal, cfg);
    drop(restore);
    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cfg: TuiConfig,
) -> Result<(), String> {
    session::archive_live(&cfg.session_dir);
    let mut app = App {
        cfg,
        entries: Vec::new(),
        messages: Vec::new(),
        input: String::new(),
        cursor: 0,
        scroll: 0,
        max_scroll: 0,
        page_size: 1,
        follow: true,
        running: false,
        compacting: false,
        cancel: Arc::new(AtomicBool::new(false)),
        events: None,
        steer: None,
        retry_after_compact: false,
        overflow_retried: false,
        input_tokens: 0,
        output_tokens: 0,
        session_input: 0,
        session_output: 0,
        turn_started: Instant::now(),
        tool_running: None,
        tool_live: None,
        history: Vec::new(),
        history_index: None,
        want_quit: false,
        ctrl_c_armed: None,
        picker: None,
        help_open: false,
        help_selected: 0,
        resume_open: false,
        resume_selected: 0,
        sessions: Vec::new(),
        catalog_query: String::new(),
        rewind_open: false,
        rewind_selected: 0,
        rewind_items: Vec::new(),
        esc_armed: None,
        resume_id: None,
    };
    if let Some(id) = app.cfg.resume.clone() {
        if id.is_empty() {
            app.open_resume();
        } else {
            app.resume(&id);
        }
    }
    loop {
        app.drain_events();
        if app.want_quit {
            app.on_exit();
            return Ok(());
        }
        terminal
            .draw(|frame| app.draw(frame))
            .map_err(|error| error.to_string())?;
        if !event::poll(Duration::from_millis(30)).map_err(|error| error.to_string())? {
            continue;
        }
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

impl App {
    fn on_exit(&mut self) {
        match self.resume_id.take() {
            Some(id) => {
                if !session::continue_archived_live(&self.cfg.session_dir, &id).unwrap_or(false) {
                    let entries = session::load_live(&self.cfg.session_dir);
                    let _ = session::continue_archived(&self.cfg.session_dir, &id, &entries);
                }
            }
            None => {
                session::archive_live(&self.cfg.session_dir);
            }
        }
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        if self.help_open {
            self.draw_help(frame);
            return;
        }
        if self.resume_open {
            self.draw_resume(frame);
            return;
        }
        if self.rewind_open {
            self.draw_rewind(frame);
            return;
        }
        self.sync_picker();
        let width = frame.area().width.max(1);
        let input_width = width.saturating_sub(2).max(1) as usize;
        let input_lines = self.input.split('\n').fold(0usize, |count, line| {
            count + line.chars().count().max(1).div_ceil(input_width)
        });
        let input_height =
            input_lines.clamp(1, frame.area().height.saturating_div(2) as usize) as u16;
        let activity_height = if self.tool_live.is_some() {
            2
        } else {
            u16::from(self.running || self.compacting)
        };
        let picker_height = self
            .picker
            .as_ref()
            .map_or(0, |picker| picker.items.len().min(6) as u16 + 2);
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
        let transcript = self.transcript(transcript_width);
        let line_count = transcript
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.chars().count())
                    .sum::<usize>()
                    .max(1)
                    .div_ceil(transcript_width)
            })
            .sum::<usize>() as u16;
        self.page_size = areas[0].height.max(1);
        self.max_scroll = line_count.saturating_sub(self.page_size);
        if self.follow {
            self.scroll = self.max_scroll;
        } else {
            self.scroll = self.scroll.min(self.max_scroll);
        }
        let transcript = Paragraph::new(transcript)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(transcript, areas[0]);
        if self.running || self.compacting {
            let elapsed = self.turn_started.elapsed().as_secs();
            let activity = if self.compacting {
                "• Compacting".to_string()
            } else if let Some(tool) = &self.tool_running {
                format!("● {tool}")
            } else {
                format!(
                    "• Thinking ({elapsed}s) (↑{} ↓{})",
                    format_tokens(self.input_tokens),
                    format_tokens(self.output_tokens)
                )
            };
            let mut lines = vec![Line::from(activity)];
            if let Some(output) = &self.tool_live {
                lines.push(Line::from(Span::styled(
                    format!("  {}", output.lines().last().unwrap_or_default()),
                    Style::default(),
                )));
            }
            frame.render_widget(Paragraph::new(lines).style(Style::default()), areas[1]);
        }
        let input = Paragraph::new(self.input_text()).wrap(Wrap { trim: false });
        frame.render_widget(input, areas[2]);
        if let Some(picker) = &self.picker {
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
        let mut status = if self.ctrl_c_armed.is_some() {
            "press ctrl+c again to exit".to_string()
        } else {
            let mut parts = vec![self.cfg.model.clone()];
            if let Some(window) = self.cfg.context_window
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
        if !self.follow {
            status.push_str(&format!(" · {}/{}", self.scroll, self.max_scroll));
        }
        frame.render_widget(Paragraph::new(status).style(Style::default()), areas[4]);
        let (cursor_row, cursor_col) = self.cursor_position(input_width);
        let visible_start = input_lines.saturating_sub(input_height as usize);
        let cursor_row = cursor_row.saturating_sub(visible_start);
        frame.set_cursor_position((
            areas[2].x + 2 + cursor_col as u16,
            areas[2].y + cursor_row as u16,
        ));
    }

    fn filtered_sessions(&self) -> Vec<&session::SessionMeta> {
        let query = self.catalog_query.to_lowercase();
        self.sessions
            .iter()
            .filter(|session| {
                query.is_empty()
                    || session.title.to_lowercase().contains(&query)
                    || session.id.to_lowercase().contains(&query)
            })
            .collect()
    }

    fn filtered_rewind_items(&self) -> Vec<app::RewindItem> {
        app::filter_rewind_items(&self.rewind_items, &self.catalog_query)
    }

    fn draw_help(&self, frame: &mut ratatui::Frame) {
        let commands = app::command_search(&self.catalog_query);
        let area = frame.area();
        let visible = area.height.saturating_sub(4) as usize;
        let start = self.help_selected.saturating_sub(visible.saturating_sub(1));
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Help {}", commands.len()),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )),
            Line::default(),
        ];
        for (index, command) in commands.iter().enumerate().skip(start).take(visible) {
            let style = if index == self.help_selected {
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
                self.catalog_query
            ),
            Style::default(),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn open_resume(&mut self) {
        self.sessions = session::list_sessions(&self.cfg.session_dir);
        self.catalog_query.clear();
        self.resume_selected = 0;
        self.resume_open = true;
        self.picker = None;
    }

    fn draw_resume(&self, frame: &mut ratatui::Frame) {
        let sessions = self.filtered_sessions();
        let area = frame.area();
        let visible = area.height.saturating_sub(4) as usize;
        let start = self
            .resume_selected
            .saturating_sub(visible.saturating_sub(1));
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Resume {}", sessions.len()),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )),
            Line::default(),
        ];
        for (index, session) in sessions.iter().enumerate().skip(start).take(visible) {
            let style = if index == self.resume_selected {
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
                self.catalog_query
            ),
            Style::default(),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn open_rewind(&mut self) {
        self.rewind_items = app::rewind_items(&self.messages);
        self.catalog_query.clear();
        self.rewind_selected = self.rewind_items.len().saturating_sub(1);
        self.rewind_open = true;
        self.picker = None;
    }

    fn draw_rewind(&self, frame: &mut ratatui::Frame) {
        let items = self.filtered_rewind_items();
        let area = frame.area();
        let visible = area.height.saturating_sub(4) as usize;
        let start = self
            .rewind_selected
            .saturating_sub(visible.saturating_sub(1));
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Rewind {}", items.len()),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )),
            Line::default(),
        ];
        for (index, item) in items.iter().enumerate().skip(start).take(visible) {
            let style = if index == self.rewind_selected {
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
                self.catalog_query
            ),
            Style::default(),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn resume(&mut self, id: &str) {
        if let Some(previous) = self.resume_id.take()
            && !session::continue_archived_live(&self.cfg.session_dir, &previous).unwrap_or(false)
        {
            let entries = session::load_live(&self.cfg.session_dir);
            let _ = session::continue_archived(&self.cfg.session_dir, &previous, &entries);
        }
        let Some((id, entries)) = app::load_session(&self.cfg.session_dir, id) else {
            self.entries
                .push(Entry::Notice(format!("no such session: {id}")));
            return;
        };
        self.resume_id = Some(id.clone());
        session::set_resume_id(&self.cfg.session_dir, &id);
        let transcript = entries
            .iter()
            .filter_map(|entry| match entry {
                session::Entry::Message { message } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.messages = session::context_messages(&entries);
        session::drop_incomplete_tool_calls(&mut self.messages);
        let usage = app::session_usage(&entries);
        self.input_tokens = usage.context_input;
        self.output_tokens = 0;
        self.session_input = usage.input;
        self.session_output = usage.output;
        self.rebuild_transcript_from(&transcript);
        if let Err(error) = session::save_live(&self.cfg.session_dir, &entries) {
            self.entries
                .push(Entry::Notice(format!("error: save session: {error}")));
        }
    }

    fn rebuild_transcript(&mut self) {
        let messages = self.messages.clone();
        self.rebuild_transcript_from(&messages);
    }

    fn rebuild_transcript_from(&mut self, messages: &[Message]) {
        self.entries.clear();
        for message in messages {
            match message.role.as_str() {
                "user" => self.entries.push(Entry::User(message.content.clone())),
                "assistant" => {
                    if !message.content.is_empty() {
                        self.entries.push(assistant_entry(message.content.clone()));
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

    fn transcript(&self, width: usize) -> Text<'static> {
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "axe",
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
                ),
                Span::styled(
                    format!(" v{VERSION} · Run /help for commands"),
                    Style::default(),
                ),
            ]),
            Line::default(),
        ];
        for entry in &self.entries {
            match entry {
                Entry::User(text) => {
                    for line in text.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("┃ ", Style::default()),
                            Span::styled(
                                line.to_string(),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    }
                }
                Entry::Assistant { rendered, .. } => {
                    for line in rendered {
                        lines.extend(wrap_markdown_line(line, width));
                    }
                }
                Entry::Tool(calls) => {
                    let style = Style::default().add_modifier(Modifier::DIM);
                    if calls.len() == 1 {
                        lines.push(Line::from(vec![
                            Span::styled("● ", style),
                            Span::styled(calls[0].clone(), style),
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
                            lines.push(Line::from(Span::styled(format!("{branch} {call}"), style)));
                        }
                    }
                }
                Entry::Notice(text) => {
                    lines.push(Line::from(Span::styled(text.clone(), Style::default())))
                }
            }
            lines.push(Line::default());
        }
        Text::from(lines)
    }

    fn input_text(&self) -> Text<'_> {
        let mut lines = Vec::new();
        for line in self.input.split('\n') {
            lines.push(Line::from(vec![
                Span::styled("┃ ", Style::default()),
                Span::raw(line),
            ]));
        }
        Text::from(lines)
    }

    fn cursor_position(&self, width: usize) -> (usize, usize) {
        let before = &self.input[..self.cursor];
        let mut row = 0;
        let mut col = 0;
        for character in before.chars() {
            if character == '\n' {
                row += 1;
                col = 0;
                continue;
            }
            if col == width {
                row += 1;
                col = 0;
            }
            col += 1;
        }
        (row, col)
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.rewind_open {
            match key.code {
                KeyCode::Esc => self.rewind_open = false,
                KeyCode::Up => {
                    self.rewind_selected = self.rewind_selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    self.rewind_selected = (self.rewind_selected + 1)
                        .min(self.filtered_rewind_items().len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(index) = self
                        .filtered_rewind_items()
                        .get(self.rewind_selected)
                        .map(|item| item.message_index)
                    {
                        self.rewind_open = false;
                        self.rewind_to(index);
                    }
                }
                _ if self.edit_catalog_query(key) => self.rewind_selected = 0,
                _ => {}
            }
            return false;
        }
        if self.resume_open {
            match key.code {
                KeyCode::Esc => self.resume_open = false,
                KeyCode::Up => {
                    self.resume_selected = self.resume_selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    self.resume_selected = (self.resume_selected + 1)
                        .min(self.filtered_sessions().len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(id) = self
                        .filtered_sessions()
                        .get(self.resume_selected)
                        .map(|session| session.id.clone())
                    {
                        self.resume_open = false;
                        self.resume(&id);
                    }
                }
                _ if self.edit_catalog_query(key) => self.resume_selected = 0,
                _ => {}
            }
            return false;
        }
        if self.help_open {
            match key.code {
                KeyCode::Esc => self.help_open = false,
                KeyCode::Up => {
                    self.help_selected = self.help_selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    self.help_selected = (self.help_selected + 1).min(
                        app::command_search(&self.catalog_query)
                            .len()
                            .saturating_sub(1),
                    );
                }
                KeyCode::Enter => {
                    if let Some(command) = app::command_search(&self.catalog_query)
                        .get(self.help_selected)
                        .map(|command| command.command)
                    {
                        self.help_open = false;
                        self.run_command(command);
                    }
                }
                _ if self.edit_catalog_query(key) => self.help_selected = 0,
                _ => {}
            }
            return false;
        }
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            self.insert('\n');
            self.picker = None;
            return false;
        }
        if self.picker.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.move_picker(true);
                    return false;
                }
                KeyCode::Down => {
                    self.move_picker(false);
                    return false;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.select_picker();
                    return false;
                }
                KeyCode::Esc => {
                    self.picker = None;
                    return false;
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Char('a') => {
                    self.cursor = 0;
                    return false;
                }
                KeyCode::Char('e') => {
                    self.cursor = self.input.len();
                    return false;
                }
                KeyCode::Char('w') => {
                    self.delete_word_left();
                    return false;
                }
                KeyCode::Char('u') => {
                    let start = self.line_start();
                    self.input.drain(start..self.cursor);
                    self.cursor = start;
                    return false;
                }
                KeyCode::Left => {
                    self.move_word_left();
                    return false;
                }
                KeyCode::Right => {
                    self.move_word_right();
                    return false;
                }
                KeyCode::Home => {
                    self.cursor = 0;
                    return false;
                }
                KeyCode::End => {
                    self.cursor = self.input.len();
                    return false;
                }
                KeyCode::Char('c') => {
                    if self.running {
                        self.cancel.store(true, Ordering::Relaxed);
                    } else if self
                        .ctrl_c_armed
                        .is_some_and(|armed| armed.elapsed() < Duration::from_millis(800))
                    {
                        return true;
                    } else if self.input.is_empty() {
                        self.ctrl_c_armed = Some(Instant::now());
                    } else {
                        self.input.clear();
                        self.cursor = 0;
                    }
                    return false;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Char(character) => self.insert(character),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => self.move_word_left(),
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => self.move_word_right(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Up if self.input.contains('\n') => self.move_line_up(),
            KeyCode::Down if self.input.contains('\n') => self.move_line_down(),
            KeyCode::Up => self.history_previous(),
            KeyCode::Down => self.history_next(),
            KeyCode::PageUp => self.scroll_up(self.page_size.saturating_sub(1)),
            KeyCode::PageDown => self.scroll_down(self.page_size.saturating_sub(1)),
            KeyCode::Esc => {
                if self
                    .esc_armed
                    .is_some_and(|armed| armed.elapsed() < Duration::from_millis(800))
                {
                    self.esc_armed = None;
                    self.open_rewind();
                } else {
                    self.esc_armed = Some(Instant::now());
                }
            }
            _ => {}
        }
        false
    }

    fn edit_catalog_query(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.catalog_query.push(character);
                true
            }
            KeyCode::Backspace => {
                self.catalog_query.pop();
                true
            }
            _ => false,
        }
    }

    fn sync_picker(&mut self) {
        if self.input.starts_with('/') && !self.input[..self.cursor].contains(char::is_whitespace) {
            let items = app::command_matches(&self.input[1..self.cursor])
                .into_iter()
                .map(|spec| spec.command.to_string())
                .collect::<Vec<_>>();
            let selected = self
                .picker
                .as_ref()
                .filter(|picker| picker.kind == PickerKind::Commands)
                .map_or(0, |picker| {
                    picker.selected.min(items.len().saturating_sub(1))
                });
            self.picker = (!items.is_empty()).then_some(Picker {
                kind: PickerKind::Commands,
                start: 0,
                end: self.cursor,
                items,
                selected,
            });
            return;
        }
        let start = self.input[..self.cursor]
            .rfind(|character: char| {
                character.is_whitespace()
                    || matches!(character, '(' | '[' | '{' | '<' | '\'' | '"' | '`')
            })
            .map_or(0, |index| index + 1);
        if self.input[start..self.cursor].starts_with('@') {
            let items = app::file_matches(&self.input[start + 1..self.cursor], &self.cfg.dir);
            let selected = self
                .picker
                .as_ref()
                .filter(|picker| picker.kind == PickerKind::Files)
                .map_or(0, |picker| {
                    picker.selected.min(items.len().saturating_sub(1))
                });
            self.picker = (!items.is_empty()).then_some(Picker {
                kind: PickerKind::Files,
                start,
                end: self.cursor,
                items,
                selected,
            });
            return;
        }
        self.picker = None;
    }

    fn move_picker(&mut self, up: bool) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let count = picker.items.len();
        if up {
            picker.selected = (picker.selected + count - 1) % count;
        } else {
            picker.selected = (picker.selected + 1) % count;
        }
    }

    fn select_picker(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(item) = picker.items.get(picker.selected) else {
            return;
        };
        let value = if picker.kind == PickerKind::Files {
            format!("@{item}")
        } else {
            item.clone()
        };
        self.input.replace_range(picker.start..picker.end, &value);
        self.cursor = picker.start + value.len();
        if picker.kind == PickerKind::Commands {
            self.submit();
        }
    }

    fn scroll_up(&mut self, rows: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(rows);
    }

    fn scroll_down(&mut self, rows: u16) {
        self.scroll = self.scroll.saturating_add(rows).min(self.max_scroll);
        self.follow = self.scroll == self.max_scroll;
    }

    fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = self
            .history_index
            .map_or(self.history.len() - 1, |index| index.saturating_sub(1));
        self.history_index = Some(index);
        self.input = self.history[index].clone();
        self.cursor = self.input.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 == self.history.len() {
            self.history_index = None;
            self.input.clear();
        } else {
            self.history_index = Some(index + 1);
            self.input = self.history[index + 1].clone();
        }
        self.cursor = self.input.len();
    }

    fn insert(&mut self, character: char) {
        self.input.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.input[..self.cursor]
            .char_indices()
            .next_back()
            .unwrap()
            .0;
        self.input.drain(start..self.cursor);
        self.cursor = start;
    }

    fn delete(&mut self) {
        if self.cursor == self.input.len() {
            return;
        }
        let end = self.cursor + self.input[self.cursor..].chars().next().unwrap().len_utf8();
        self.input.drain(self.cursor..end);
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.input[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.input[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn move_word_left(&mut self) {
        while self.cursor > 0 {
            let (index, character) = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .unwrap();
            if !character.is_whitespace() {
                break;
            }
            self.cursor = index;
        }
        while self.cursor > 0 {
            let (index, character) = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .unwrap();
            if character.is_whitespace() {
                break;
            }
            self.cursor = index;
        }
    }

    fn move_word_right(&mut self) {
        while let Some(character) = self.input[self.cursor..].chars().next() {
            if !character.is_whitespace() {
                break;
            }
            self.cursor += character.len_utf8();
        }
        while let Some(character) = self.input[self.cursor..].chars().next() {
            if character.is_whitespace() {
                break;
            }
            self.cursor += character.len_utf8();
        }
    }

    fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        self.input.drain(self.cursor..end);
    }

    fn move_line_up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            return;
        }
        let column = self.input[start..self.cursor].chars().count();
        let previous_end = start - 1;
        let previous_start = self.input[..previous_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = byte_at_column(&self.input, previous_start, previous_end, column);
    }

    fn move_line_down(&mut self) {
        let end = self.line_end();
        if end == self.input.len() {
            return;
        }
        let column = self.input[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.input[next_start..]
            .find('\n')
            .map_or(self.input.len(), |index| next_start + index);
        self.cursor = byte_at_column(&self.input, next_start, next_end, column);
    }

    fn line_start(&self) -> usize {
        self.input[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |index| self.cursor + index)
    }

    fn submit(&mut self) {
        if self.input.trim().is_empty() {
            return;
        }
        if self.input.starts_with('/') {
            let command = std::mem::take(&mut self.input);
            self.cursor = 0;
            self.run_command(&command);
            return;
        }
        let content = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_index = None;
        self.history.push(content.clone());
        self.entries.push(Entry::User(content.clone()));
        if self.running {
            if let Some(steer) = &self.steer {
                let _ = steer.send(content);
            }
            return;
        }
        self.messages.push(Message {
            role: "user".into(),
            content,
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
        });
        let entry = session::Entry::Message {
            message: self.messages.last().unwrap().clone(),
        };
        if let Err(error) = session::append_live(&self.cfg.session_dir, &[entry]) {
            self.entries
                .push(Entry::Notice(format!("error: save session: {error}")));
        }
        self.follow = true;
        self.start_turn();
    }

    fn run_command(&mut self, input: &str) {
        let argument = input.split_once(' ').map(|(_, argument)| argument.trim());
        let command = app::parse_command(input);
        if self.running
            && matches!(
                command,
                Some(
                    app::Command::New
                        | app::Command::Resume
                        | app::Command::Rewind
                        | app::Command::Compact
                )
            )
        {
            self.entries.push(Entry::Notice(
                "agent is running; ctrl+c interrupts it first".into(),
            ));
            return;
        }
        match command {
            Some(app::Command::Quit) => self.want_quit = true,
            Some(app::Command::Help) => {
                self.catalog_query.clear();
                self.help_open = true;
                self.help_selected = 0;
                self.picker = None;
            }
            Some(app::Command::New) => {
                session::archive_live(&self.cfg.session_dir);
                self.clear_session();
            }
            Some(app::Command::Resume) => match argument {
                Some(id) if !id.is_empty() => self.resume(id),
                _ => self.open_resume(),
            },
            Some(app::Command::Rewind) => self.open_rewind(),
            Some(app::Command::Compact) => self.start_compaction(),
            Some(app::Command::Copy) => self.copy_last(),
            None => self
                .entries
                .push(Entry::Notice(format!("unknown command: {input}"))),
        }
    }

    fn clear_session(&mut self) {
        self.entries.clear();
        self.messages.clear();
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.session_input = 0;
        self.session_output = 0;
        self.history.clear();
        self.history_index = None;
        self.follow = true;
    }

    fn rewind_to(&mut self, index: usize) {
        let entries = app::rewind_entries(&self.messages, index);
        if let Err(error) = session::save_live(&self.cfg.session_dir, &entries) {
            self.entries
                .push(Entry::Notice(format!("error: save session: {error}")));
            return;
        }
        self.messages = session::context_messages(&entries);
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.session_input = 0;
        self.session_output = 0;
        self.rebuild_transcript();
        self.entries.push(Entry::Notice(format!(
            "rewound · {} messages remaining",
            self.messages.len()
        )));
    }

    fn copy_last(&mut self) {
        let Some(text) = app::last_assistant_response(&self.messages) else {
            self.entries
                .push(Entry::Notice("no assistant response to copy".into()));
            return;
        };
        let encoded = b64_encode(text.as_bytes());
        print!("\u{1b}]52;c;{encoded}\u{1b}\\");
        let _ = io::stdout().flush();
        self.entries
            .push(Entry::Notice("copied last response".into()));
    }

    fn start_compaction(&mut self) {
        if self.compacting {
            self.entries
                .push(Entry::Notice("already compacting".into()));
            return;
        }
        if self.messages.len() < 4 {
            self.entries
                .push(Entry::Notice("session too small to compact".into()));
            return;
        }
        let provider = OpenAI::new(self.cfg.base.clone(), self.cfg.api_key.clone());
        let model = self.cfg.model.clone();
        let entries = session::load_live(&self.cfg.session_dir);
        let (sender, receiver) = mpsc::channel();
        self.events = Some(receiver);
        self.compacting = true;
        self.turn_started = Instant::now();
        std::thread::spawn(move || {
            let result = session::compact(&provider, &model, &entries).map(
                |(summary, tokens_before, retained)| (summary, tokens_before, retained, entries),
            );
            let _ = sender.send(TurnEvent::Compacted(result));
        });
    }

    fn start_turn(&mut self) {
        let messages = self.messages.clone();
        let provider = OpenAI::new(self.cfg.base.clone(), self.cfg.api_key.clone());
        let model = self.cfg.model.clone();
        let system = self.cfg.system.clone();
        let tools = build_tools(&self.cfg.dir);
        let threshold = self
            .cfg
            .context_window
            .map(|window| window.saturating_sub(16384));
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let (steer, steer_receiver) = mpsc::channel();
        self.events = Some(receiver);
        self.steer = Some(steer);
        self.running = true;
        self.turn_started = Instant::now();
        std::thread::spawn(move || {
            let end = {
                let mut sink = RatatuiSink {
                    sender: &sender,
                    steer: steer_receiver,
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
                    &messages,
                    &cancel,
                    &mut sink,
                )
            };
            let compact = matches!(end.outcome, Outcome::Compact);
            let error = match end.outcome {
                Outcome::Done | Outcome::Cancelled | Outcome::Compact => None,
                Outcome::MaxTurns => Some("stopped: max turns reached".into()),
                Outcome::Failed(error) => Some(error),
            };
            let _ = sender.send(TurnEvent::End {
                messages: end.messages,
                usage: end.usage,
                context: end.context,
                error,
                compact,
            });
        });
    }

    fn drain_events(&mut self) {
        let Some(receiver) = self.events.take() else {
            return;
        };
        let mut compact_after = false;
        let mut turn_after = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                TurnEvent::AssistantDelta(delta) => {
                    if let Some(Entry::Assistant { source, rendered }) = self.entries.last_mut() {
                        source.push_str(&delta);
                        *rendered = render_markdown(source);
                    } else {
                        self.entries.push(assistant_entry(delta));
                    }
                }
                TurnEvent::ToolStart(label) => {
                    self.tool_running = Some(label);
                    self.tool_live = None;
                }
                TurnEvent::ToolDelta(output) => self.tool_live = Some(output),
                TurnEvent::ToolResult(label) => {
                    self.tool_running = None;
                    self.tool_live = None;
                    if let Some(Entry::Tool(calls)) = self.entries.last_mut() {
                        calls.push(label);
                    } else {
                        self.entries.push(Entry::Tool(vec![label]));
                    }
                }
                TurnEvent::Tokens(usage) => {
                    if usage.input > 0 {
                        self.input_tokens = usage.input;
                    }
                    self.output_tokens = usage.output;
                }
                TurnEvent::Compacted(result) => {
                    self.compacting = false;
                    match result {
                        Ok((summary, tokens_before, retained, mut entries)) => {
                            let entry = session::Entry::Compaction {
                                summary,
                                tokens_before,
                                timestamp: session::now_ms(),
                                retained,
                            };
                            if let Err(error) = session::append_live(
                                &self.cfg.session_dir,
                                std::slice::from_ref(&entry),
                            ) {
                                self.entries
                                    .push(Entry::Notice(format!("error: save session: {error}")));
                                continue;
                            }
                            entries.push(entry);
                            self.messages = session::context_messages(&entries);
                            self.input_tokens = 0;
                            self.output_tokens = 0;
                            self.rebuild_transcript();
                            self.entries.push(Entry::Notice("compacted".into()));
                            turn_after = self.retry_after_compact;
                            self.retry_after_compact = false;
                        }
                        Err(error) => self
                            .entries
                            .push(Entry::Notice(format!("compaction failed: {error}"))),
                    }
                }
                TurnEvent::End {
                    messages,
                    usage,
                    context,
                    error,
                    compact,
                } => {
                    let mut entries: Vec<_> = messages[self.messages.len()..]
                        .iter()
                        .cloned()
                        .map(|message| session::Entry::Message { message })
                        .collect();
                    if usage.input > 0 || usage.output > 0 {
                        entries.push(session::Entry::Usage {
                            input: usage.input,
                            output: usage.output,
                            cached_input: context.cached_input,
                            context_input: context.input,
                            context_output: context.output,
                        });
                    }
                    if let Err(error) = session::append_live(&self.cfg.session_dir, &entries) {
                        self.entries
                            .push(Entry::Notice(format!("error: save session: {error}")));
                    }
                    self.messages = messages;
                    self.session_input += usage.input;
                    self.session_output += usage.output;
                    self.running = false;
                    self.steer = None;
                    compact_after = compact
                        || error.as_deref().is_some_and(session::is_overflow_error)
                            && !self.overflow_retried;
                    if compact_after {
                        self.retry_after_compact = true;
                        self.overflow_retried = true;
                    } else if let Some(error) = error {
                        self.entries.push(Entry::Notice(format!("error: {error}")));
                    }
                }
            }
        }
        if compact_after {
            self.start_compaction();
        } else if turn_after {
            self.start_turn();
        } else if self.running || self.compacting {
            self.events = Some(receiver);
        }
    }
}

fn assistant_entry(source: String) -> Entry {
    let rendered = render_markdown(&source);
    Entry::Assistant { source, rendered }
}

fn render_markdown(markdown: &str) -> Vec<Line<'static>> {
    tui_markdown::from_str(markdown)
        .lines
        .into_iter()
        .map(|line| {
            let mut spans = line
                .spans
                .into_iter()
                .map(|span| {
                    let style = if span.style.fg.is_some() {
                        span.style
                    } else if span.style.add_modifier.contains(Modifier::BOLD) {
                        span.style.fg(Color::Cyan)
                    } else if span.style.add_modifier.contains(Modifier::ITALIC) {
                        span.style.fg(Color::Magenta)
                    } else {
                        span.style
                    };
                    Span::styled(span.content.into_owned(), style)
                })
                .collect::<Vec<_>>();
            if !spans.is_empty() {
                spans.insert(0, Span::raw("  "));
            }
            Line::from(spans).style(line.style)
        })
        .collect()
}

fn wrap_markdown_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let characters = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(move |character| (character, span.style))
        })
        .collect::<Vec<_>>();
    if characters.len() <= width || width == 0 {
        return vec![line.clone()];
    }
    let marker_width = line.spans.get(1).map_or(0, |span| {
        let marker = span.content.as_ref();
        let trimmed = marker.trim_start();
        let ordered = trimmed.split_once(". ").is_some_and(|(number, rest)| {
            rest.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        });
        if marker.ends_with("- ") || marker.ends_with("] ") || ordered {
            marker.chars().count()
        } else {
            0
        }
    });
    let continuation_width = 2 + marker_width;
    let mut lines = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < characters.len() {
        let indent = if first {
            0
        } else {
            continuation_width.min(width.saturating_sub(1))
        };
        let capacity = width.saturating_sub(indent).max(1);
        let limit = (start + capacity).min(characters.len());
        let mut end = limit;
        if limit < characters.len() {
            if let Some(offset) = characters[start..limit]
                .iter()
                .rposition(|(character, _)| character.is_whitespace())
            {
                end = start + offset;
            }
            if end == start {
                end = limit;
            }
        }
        let mut spans = Vec::new();
        if indent > 0 {
            spans.push(Span::raw(" ".repeat(indent)));
        }
        for (character, style) in &characters[start..end] {
            if spans.last().is_some_and(|span| span.style == *style) {
                spans.last_mut().unwrap().content.to_mut().push(*character);
            } else {
                spans.push(Span::styled(character.to_string(), *style));
            }
        }
        lines.push(Line::from(spans).style(line.style));
        start = end;
        while start < characters.len() && characters[start].0.is_whitespace() {
            start += 1;
        }
        first = false;
    }
    lines
}

fn format_tokens(tokens: usize) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{}k", tokens / 1000)
    }
}

fn age(updated: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let elapsed = (now - updated).max(0) as u64;
    if elapsed < 60_000 {
        "now".into()
    } else if elapsed < 3_600_000 {
        format!("{}m", elapsed / 60_000)
    } else if elapsed < 86_400_000 {
        format!("{}h", elapsed / 3_600_000)
    } else {
        format!("{}d", elapsed / 86_400_000)
    }
}

fn byte_at_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(index, _)| start + index)
}

fn b64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        output.push(TABLE[(bytes[0] >> 2) as usize] as char);
        output.push(TABLE[(((bytes[0] & 3) << 4) | (bytes[1] >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((bytes[1] & 15) << 2) | (bytes[2] >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(bytes[2] & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

struct RatatuiSink<'a> {
    sender: &'a Sender<TurnEvent>,
    steer: Receiver<String>,
    threshold: Option<usize>,
}

impl Sink for RatatuiSink<'_> {
    fn assistant_delta(&mut self, text: &str) {
        let _ = self.sender.send(TurnEvent::AssistantDelta(text.into()));
    }

    fn tool_start(&mut self, call: &ToolCall) {
        let _ = self
            .sender
            .send(TurnEvent::ToolStart(app::tool_label(call, true)));
    }

    fn tool_delta(&mut self, _call: &ToolCall, text: &str) {
        let _ = self.sender.send(TurnEvent::ToolDelta(text.into()));
    }

    fn tool_result(&mut self, call: &ToolCall) {
        let _ = self
            .sender
            .send(TurnEvent::ToolResult(app::tool_label(call, false)));
    }

    fn tokens(&mut self, input: usize, output: usize, cached_input: usize) {
        let _ = self.sender.send(TurnEvent::Tokens(Usage {
            input,
            output,
            cached_input,
        }));
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
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    #[test]
    fn markdown_render_is_owned_and_visible() {
        let rendered = {
            let source = "# Heading\n\n**bold**".to_string();
            render_markdown(&source)
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
        let normal = render_markdown("alpha beta gamma");
        let normal = wrap_markdown_line(&normal[0], 12);
        assert_eq!(line_text(&normal[1]), "  beta gamma");

        let list = render_markdown("- alpha beta gamma");
        let list = wrap_markdown_line(&list[0], 12);
        assert_eq!(line_text(&list[1]), "    beta");
    }

    #[test]
    fn markdown_wrap_indents_every_marker_kind() {
        // Ordered ("1. ") and task-list ("- [x] ") markers carry the same
        // continuation indent as the bullet marker.
        let ordered = render_markdown("1. alpha beta gamma");
        let ordered = wrap_markdown_line(&ordered[0], 12);
        assert_eq!(line_text(&ordered[1]), "     beta");

        let task = render_markdown("- [x] alpha beta gamma");
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
}
