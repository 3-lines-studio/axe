use super::App;
use super::render::{assistant_entry, byte_at_column, dropped_image_path};
use super::runtime::TurnEvent;
use crate::app;
use crate::session;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub(super) enum Entry {
    User {
        text: String,
        images: Vec<String>,
    },
    Assistant {
        source: String,
        rendered: Vec<Line<'static>>,
        width: usize,
    },
    Tool(Vec<String>),
    Notice(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PickerKind {
    Commands,
    Files,
}

pub(super) struct Picker {
    pub(super) kind: PickerKind,
    start: usize,
    end: usize,
    pub(super) items: Vec<String>,
    pub(super) selected: usize,
}

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
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
                KeyCode::Char('j') => {
                    self.insert('\n');
                    self.picker = None;
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

    pub(super) fn edit_catalog_query(&mut self, key: KeyEvent) -> bool {
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

    pub(super) fn sync_picker(&mut self) {
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

    pub(super) fn move_picker(&mut self, up: bool) {
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

    pub(super) fn select_picker(&mut self) {
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

    pub(super) fn scroll_up(&mut self, rows: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(rows);
    }

    pub(super) fn scroll_down(&mut self, rows: u16) {
        self.scroll = self.scroll.saturating_add(rows).min(self.max_scroll);
        self.follow = self.scroll == self.max_scroll;
    }

    pub(super) fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.input.is_empty()
            && self.attachments.is_empty()
            && let Some(path) = dropped_image_path(&text)
        {
            self.attach(Some(path.as_str()));
            return;
        }
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub(super) fn history_previous(&mut self) {
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

    pub(super) fn history_next(&mut self) {
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

    pub(super) fn insert(&mut self, character: char) {
        self.input.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub(super) fn backspace(&mut self) {
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

    pub(super) fn delete(&mut self) {
        if self.cursor == self.input.len() {
            return;
        }
        let end = self.cursor + self.input[self.cursor..].chars().next().unwrap().len_utf8();
        self.input.drain(self.cursor..end);
    }

    pub(super) fn move_left(&mut self) {
        if let Some((index, _)) = self.input[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    pub(super) fn move_right(&mut self) {
        if let Some(character) = self.input[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    pub(super) fn move_word_left(&mut self) {
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

    pub(super) fn move_word_right(&mut self) {
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

    pub(super) fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        self.input.drain(self.cursor..end);
    }

    pub(super) fn move_line_up(&mut self) {
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

    pub(super) fn move_line_down(&mut self) {
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

    pub(super) fn line_start(&self) -> usize {
        self.input[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    pub(super) fn line_end(&self) -> usize {
        self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |index| self.cursor + index)
    }

    pub(super) fn drain_events(&mut self) -> bool {
        let Some(receiver) = self.events.take() else {
            return false;
        };
        let mut changed = false;
        let mut compact_after = false;
        let mut turn_after = false;
        while let Ok(event) = receiver.try_recv() {
            changed = true;
            match event {
                TurnEvent::AssistantDelta(delta) => {
                    if let Some(Entry::Assistant { source, .. }) = self.entries.last_mut() {
                        source.push_str(&delta);
                        self.markdown_dirty = true;
                    } else {
                        self.entries.push(assistant_entry(delta, self.width));
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
                            if let Err(error) = self.cfg.store.append(std::slice::from_ref(&entry))
                            {
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
                    let entries: Vec<_> = messages[self.messages.len()..]
                        .iter()
                        .cloned()
                        .map(|message| session::Entry::Message { message })
                        .collect();
                    let mut state = self.session_state.clone();
                    state.record(&usage, &context);
                    if let Err(error) = self.cfg.store.append_with_state(&entries, state.clone()) {
                        self.entries
                            .push(Entry::Notice(format!("error: save session: {error}")));
                    }
                    self.messages = messages;
                    self.session_state = state;
                    self.session_input = self.session_state.input;
                    self.session_output = self.session_state.output;
                    self.input_tokens = self.session_state.context_input;
                    self.output_tokens = self.session_state.context_output;
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
        changed
    }
}
