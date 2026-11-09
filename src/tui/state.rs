use super::render::{assistant_entry, byte_at_column, dropped_image_path};
use super::runtime::TurnEvent;
use super::status::{self, State};
use super::{
    App, CatalogKey, Next, Overlay, Scroll, Selection, ToolActivity, Turn, filtered_sessions,
};
use crate::app;
use crate::run::Outcome;
use crate::session;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use std::sync::atomic::Ordering;

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
    Skills,
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
        if key.code != KeyCode::Esc {
            self.esc.clear();
        }
        if self.handle_overlay(key) {
            return false;
        }
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            self.insert('\n');
            self.overlay = Overlay::None;
            return false;
        }
        if matches!(self.overlay, Overlay::Picker(_)) {
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
                    self.overlay = Overlay::None;
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
                    self.overlay = Overlay::None;
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
                KeyCode::Char('c') => return self.interrupt(),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => self.submit(),
            // Herdr turns ctrl+left and ctrl+right into readline's alt+b and
            // alt+f before the app sees them.
            KeyCode::Char('b' | 'B') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_word_left()
            }
            KeyCode::Char('f' | 'F') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_word_right()
            }
            KeyCode::Char(character) => self.insert(character),
            KeyCode::Backspace
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.delete_word_left()
            }
            KeyCode::Delete
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.delete_word_right()
            }
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
                if self.esc.armed() {
                    self.esc.clear();
                    self.open_rewind();
                } else {
                    self.esc.arm();
                }
            }
            _ => {}
        }
        false
    }

    fn interrupt(&mut self) -> bool {
        if matches!(self.turn, Turn::Running { .. }) {
            self.cancel.store(true, Ordering::Relaxed);
            return false;
        }
        if self.ctrl_c.armed() {
            return true;
        }
        if self.input.is_empty() {
            self.ctrl_c.arm();
        } else {
            self.input.clear();
            self.cursor = 0;
        }
        false
    }

    fn handle_overlay(&mut self, key: KeyEvent) -> bool {
        let count = self.overlay_count();
        let action = match &mut self.overlay {
            Overlay::None | Overlay::Picker(_) | Overlay::RewindPending => return false,
            Overlay::Help(catalog) => catalog.key(key, count),
            Overlay::Resume { catalog, .. } => catalog.key(key, count),
            Overlay::Rewind { catalog, .. } => catalog.key(key, count),
        };
        match action {
            CatalogKey::Keep => {}
            CatalogKey::Close => self.overlay = Overlay::None,
            CatalogKey::Select => self.select_overlay(),
        }
        true
    }

    fn overlay_count(&self) -> usize {
        match &self.overlay {
            Overlay::Help(catalog) => app::command_search(&catalog.query).len(),
            Overlay::Resume { catalog, sessions } => {
                filtered_sessions(sessions, &catalog.query).len()
            }
            Overlay::Rewind { catalog, items } => {
                app::filter_rewind_items(items, &catalog.query).len()
            }
            _ => 0,
        }
    }

    fn selection(&self) -> Option<Selection> {
        match &self.overlay {
            Overlay::Help(catalog) => app::command_search(&catalog.query)
                .get(catalog.selected)
                .map(|spec| Selection::Command(spec.action)),
            Overlay::Resume { catalog, sessions } => filtered_sessions(sessions, &catalog.query)
                .get(catalog.selected)
                .map(|session| Selection::Session(session.id.clone())),
            Overlay::Rewind { catalog, items } => app::filter_rewind_items(items, &catalog.query)
                .get(catalog.selected)
                .map(|item| Selection::Rewind(item.message_index)),
            _ => None,
        }
    }

    fn select_overlay(&mut self) {
        let Some(selection) = self.selection() else {
            return;
        };
        self.overlay = Overlay::None;
        match selection {
            Selection::Command(command) => self.execute(command, None),
            Selection::Session(id) => self.resume(&id),
            Selection::Rewind(index) => self.rewind_to(index),
        }
    }

    pub(super) fn sync_picker(&mut self) {
        if !matches!(self.overlay, Overlay::None | Overlay::Picker(_)) {
            return;
        }
        if self.input.starts_with('/') && !self.input[..self.cursor].contains(char::is_whitespace) {
            let items = app::command_matches(&self.input[1..self.cursor])
                .into_iter()
                .map(|spec| spec.command.to_string())
                .collect::<Vec<_>>();
            let selected = self.picker_selection(PickerKind::Commands, &items);
            self.set_picker(PickerKind::Commands, 0, self.cursor, items, selected);
            return;
        }
        if let Some(prefix) = app::skill_query(&self.input[..self.cursor]) {
            let items = app::skill_matches(&self.skills, prefix);
            let selected = self.picker_selection(PickerKind::Skills, &items);
            self.set_picker(
                PickerKind::Skills,
                self.cursor - prefix.len(),
                self.cursor,
                items,
                selected,
            );
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
            let selected = self.picker_selection(PickerKind::Files, &items);
            self.set_picker(PickerKind::Files, start, self.cursor, items, selected);
            return;
        }
        self.overlay = Overlay::None;
    }

    fn picker_selection(&self, kind: PickerKind, items: &[String]) -> usize {
        match &self.overlay {
            Overlay::Picker(picker) if picker.kind == kind => {
                picker.selected.min(items.len().saturating_sub(1))
            }
            _ => 0,
        }
    }

    fn set_picker(
        &mut self,
        kind: PickerKind,
        start: usize,
        end: usize,
        items: Vec<String>,
        selected: usize,
    ) {
        self.overlay = if items.is_empty() {
            Overlay::None
        } else {
            Overlay::Picker(Picker {
                kind,
                start,
                end,
                items,
                selected,
            })
        };
    }

    pub(super) fn move_picker(&mut self, up: bool) {
        let Overlay::Picker(picker) = &mut self.overlay else {
            return;
        };
        let count = picker.items.len();
        if count == 0 {
            return;
        }
        if up {
            picker.selected = (picker.selected + count - 1) % count;
        } else {
            picker.selected = (picker.selected + 1) % count;
        }
    }

    pub(super) fn select_picker(&mut self) {
        let picker = match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::Picker(picker) => picker,
            other => {
                self.overlay = other;
                return;
            }
        };
        let Some(item) = picker.items.get(picker.selected) else {
            return;
        };
        let value = match picker.kind {
            PickerKind::Files => format!("@{item}"),
            PickerKind::Skills => format!("{item} "),
            PickerKind::Commands => item.clone(),
        };
        self.input.replace_range(picker.start..picker.end, &value);
        self.cursor = picker.start + value.len();
        if picker.kind == PickerKind::Commands {
            self.submit();
        }
    }

    pub(super) fn scroll_up(&mut self, rows: u16) {
        self.scroll = Scroll::At(self.scroll.row(self.max_scroll).saturating_sub(rows));
    }

    pub(super) fn scroll_down(&mut self, rows: u16) {
        let max = self.max_scroll;
        let row = self.scroll.row(max).saturating_add(rows).min(max);
        self.scroll = if row == max {
            Scroll::Bottom
        } else {
            Scroll::At(row)
        };
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

    pub(super) fn delete_word_right(&mut self) {
        let start = self.cursor;
        self.move_word_right();
        let end = self.cursor;
        self.input.drain(start..end);
        self.cursor = start;
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

    pub(super) fn last_assistant_source(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::Assistant { source, .. } if !source.is_empty() => Some(source.as_str()),
            _ => None,
        })
    }

    pub(super) fn drain_events(&mut self) -> bool {
        let turn = std::mem::replace(&mut self.turn, Turn::Idle);
        let (events, base, resume) = match &turn {
            Turn::Idle => return false,
            Turn::Running { events, base, .. } => (events, *base, false),
            Turn::Compacting { events, resume } => (events, 0, *resume),
        };
        let mut changed = false;
        let mut next = Next::Keep;
        while let Ok(event) = events.try_recv() {
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
                    self.tool = ToolActivity::Running {
                        label,
                        output: None,
                    };
                }
                TurnEvent::ToolDelta(output) => {
                    if let ToolActivity::Running { output: live, .. } = &mut self.tool {
                        *live = Some(output);
                    }
                }
                TurnEvent::ToolResult(label) => {
                    self.transcript.invalidate();
                    self.tool = ToolActivity::Idle;
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
                    self.transcript.invalidate();
                    next = match result {
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
                                Next::Idle
                            } else {
                                entries.push(entry);
                                self.messages = session::context_messages(&entries);
                                self.input_tokens = 0;
                                self.output_tokens = 0;
                                self.rebuild_transcript();
                                self.entries.push(Entry::Notice("compacted".into()));
                                if resume { Next::Turn } else { Next::Idle }
                            }
                        }
                        Err(error) => {
                            self.entries
                                .push(Entry::Notice(format!("compaction failed: {error}")));
                            Next::Idle
                        }
                    };
                }
                TurnEvent::End {
                    messages,
                    usage,
                    context,
                    outcome,
                } => {
                    self.transcript.invalidate();
                    self.tool = ToolActivity::Idle;
                    let entries: Vec<_> = messages[base..]
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
                    if matches!(self.overlay, Overlay::RewindPending) {
                        self.open_rewind();
                        next = Next::Idle;
                        continue;
                    }
                    next = match outcome {
                        Outcome::Done | Outcome::Cancelled => Next::Idle,
                        Outcome::Compact => Next::Compact { resume: true },
                        Outcome::MaxTurns => {
                            self.entries
                                .push(Entry::Notice("error: stopped: max turns reached".into()));
                            Next::Failed
                        }
                        Outcome::Failed(error)
                            if session::is_overflow_error(&error) && !self.overflow_retried =>
                        {
                            self.overflow_retried = true;
                            Next::Compact { resume: true }
                        }
                        Outcome::Failed(error) => {
                            self.entries.push(Entry::Notice(format!("error: {error}")));
                            Next::Failed
                        }
                    };
                }
            }
        }
        if matches!(next, Next::Keep)
            && matches!(
                events.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            )
        {
            next = Next::Lost;
        }
        match next {
            Next::Keep => self.turn = turn,
            Next::Idle => status::report(State::Idle),
            Next::Failed => status::report(State::Error),
            Next::Lost => {
                self.entries.push(Entry::Notice(
                    "error: the turn ended without a result".into(),
                ));
                self.transcript.invalidate();
                self.tool = ToolActivity::Idle;
                status::report(State::Error);
            }
            Next::Compact { resume } => self.start_compaction(resume),
            Next::Turn => self.start_turn(),
        }
        changed
    }
}
