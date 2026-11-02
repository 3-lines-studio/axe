use crate::app;
use crate::session::{self, SessionState, Store};
use crate::{Image, Message};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
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

use render::*;
use runtime::TurnEvent;
use state::{Entry, Picker};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct TuiConfig {
    pub base: String,
    pub model: String,
    pub spec: crate::ModelSpec,
    pub system: String,
    pub dir: String,
    pub store: Arc<dyn Store>,
    pub api_key: String,
    pub resume: Option<String>,
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
    let result = run_app(&mut terminal, cfg);
    drop(restore);
    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cfg: TuiConfig,
) -> Result<(), String> {
    cfg.store.archive();
    let mut app = App {
        cfg,
        entries: Vec::new(),
        messages: Vec::new(),
        input: String::new(),
        cursor: 0,
        attachments: Vec::new(),
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
        session_state: SessionState::default(),
        turn_started: Instant::now(),
        tool_running: None,
        tool_live: None,
        width: 80,
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
        markdown_dirty: false,
        turn_base_len: 0,
        transcript_dirty_full: true,
        transcript_cache_width: 0,
        transcript_cache_entries: 0,
        transcript_cache: Vec::new(),
        pending_rewind: false,
    };
    if let Some(id) = app.cfg.resume.clone() {
        if id.is_empty() {
            app.open_resume();
        } else {
            app.resume(&id);
        }
    }
    let mut dirty = true;
    let mut last_tick = Instant::now();
    loop {
        if app.drain_events() {
            dirty = true;
        }
        if app.want_quit {
            app.on_exit();
            return Ok(());
        }
        if (app.running || app.compacting) && last_tick.elapsed() >= Duration::from_secs(1) {
            dirty = true;
            last_tick = Instant::now();
        }
        if dirty {
            terminal
                .draw(|frame| app.draw(frame))
                .map_err(|error| error.to_string())?;
            dirty = false;
        }
        let wait = if app.running || app.compacting {
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
    session_state: SessionState,
    turn_started: Instant,
    tool_running: Option<String>,
    tool_live: Option<String>,
    width: usize,
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
    markdown_dirty: bool,
    turn_base_len: usize,
    transcript_dirty_full: bool,
    transcript_cache_width: usize,
    transcript_cache_entries: usize,
    transcript_cache: Vec<Line<'static>>,
    pending_rewind: bool,
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

fn push_transcript_entry(lines: &mut Vec<Line<'static>>, entry: &Entry, width: usize) {
    match entry {
        Entry::User { text, images } => {
            for label in images {
                lines.push(Line::from(vec![
                    Span::styled("┃ ", Style::default()),
                    Span::styled(
                        format!("[image {label}]"),
                        Style::default().add_modifier(Modifier::DIM),
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
            let style = Style::default().add_modifier(Modifier::DIM);
            if calls.len() == 1 {
                lines.push(Line::from(vec![
                    Span::styled("● ", style),
                    Span::styled(truncate(&calls[0], width.saturating_sub(2)), style),
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
                        format!("{branch} {}", truncate(call, width.saturating_sub(2))),
                        style,
                    )));
                }
            }
        }
        Entry::Notice(text) => lines.push(Line::from(Span::styled(text.clone(), Style::default()))),
    }
    lines.push(Line::default());
}

impl App {
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
        let input_lines = self
            .input
            .split('\n')
            .map(|line| wrap_input_line(line, input_width).len())
            .sum::<usize>();
        let composer_lines = input_lines + self.attachments.len();
        let input_height =
            composer_lines.clamp(1, frame.area().height.saturating_div(2) as usize) as u16;
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
        self.width = transcript_width;
        self.sync_markdown_width(transcript_width);
        let transcript = self.transcript(transcript_width);
        let transcript = Paragraph::new(transcript).wrap(Wrap { trim: false });
        let line_count = transcript.line_count(areas[0].width.max(1)) as u16;
        self.page_size = areas[0].height.max(1);
        self.max_scroll = line_count.saturating_sub(self.page_size);
        if self.follow {
            self.scroll = self.max_scroll;
        } else {
            self.scroll = self.scroll.min(self.max_scroll);
        }
        let transcript = transcript.scroll((self.scroll, 0));
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
        let input = Paragraph::new(self.input_text(input_width));
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
        if !self.follow {
            status.push_str(&format!(" · {}/{}", self.scroll, self.max_scroll));
        }
        frame.render_widget(Clear, areas[4]);
        frame.render_widget(Paragraph::new(status).style(Style::default()), areas[4]);
        let (cursor_row, cursor_col) = self.cursor_position(input_width);
        let cursor_row = self.attachments.len() + cursor_row;
        let visible_start = composer_lines.saturating_sub(input_height as usize);
        let cursor_row = cursor_row
            .saturating_sub(visible_start)
            .min(input_height.saturating_sub(1) as usize);
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
        if self.running {
            self.pending_rewind = true;
            return;
        }
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

    fn rebuild_transcript(&mut self) {
        self.transcript_dirty_full = true;
        let messages = self.messages.clone();
        self.rebuild_transcript_from(&messages);
    }

    fn rebuild_transcript_from(&mut self, messages: &[Message]) {
        self.transcript_dirty_full = true;
        self.entries.clear();
        for message in messages {
            match message.role.as_str() {
                "user" => self.entries.push(Entry::User {
                    text: message.content.clone(),
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

    fn transcript(&mut self, width: usize) -> Text<'static> {
        let last_is_assistant = matches!(self.entries.last(), Some(Entry::Assistant { .. }));
        let prefix = self.entries.len() - usize::from(last_is_assistant);
        let reset = self.transcript_dirty_full
            || self.transcript_cache_width != width
            || self.transcript_cache_entries > prefix;
        if reset {
            self.transcript_cache.clear();
            push_transcript_header(&mut self.transcript_cache);
            self.transcript_cache_width = width;
            self.transcript_cache_entries = 0;
            self.transcript_dirty_full = false;
        }
        while self.transcript_cache_entries < prefix {
            let entry = &self.entries[self.transcript_cache_entries];
            push_transcript_entry(&mut self.transcript_cache, entry, width);
            self.transcript_cache_entries += 1;
        }
        let mut lines = self.transcript_cache.clone();
        if last_is_assistant {
            push_transcript_entry(&mut lines, &self.entries[prefix], width);
        }
        Text::from(lines)
    }

    fn input_text(&self, width: usize) -> Text<'static> {
        let mut lines = Vec::new();
        for image in &self.attachments {
            lines.push(Line::from(vec![
                Span::styled("┃ ", Style::default()),
                Span::styled(
                    format!("[image {}]", crate::image::label(image)),
                    Style::default().add_modifier(Modifier::DIM),
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
    fn truncate_shortens_with_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abcdefghij", 0), "…");
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
