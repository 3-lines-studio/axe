use super::App;
use super::runtime::{RatatuiObserver, TuiControl, TurnEvent};
use super::state::Entry;
use super::status::{self, State};
use super::{Catalog, Overlay, SessionOrigin, Turn};
use crate::Message;
use crate::app;
use crate::openai::OpenAI;
use crate::run::{self, RunOptions};
use crate::session::{self, SessionState};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Instant;

impl App {
    pub(super) fn provider(&self) -> OpenAI {
        OpenAI::new(self.cfg.base.clone(), self.cfg.api_key.clone()).effort("max")
    }

    pub(super) fn tools(&self) -> Vec<crate::Tool> {
        crate::tools::build_tools_for(
            &self.cfg.dir,
            crate::web::Web::for_key(&self.cfg.base, &self.cfg.api_key),
        )
    }

    pub(super) fn on_exit(&mut self) {
        match &self.origin {
            SessionOrigin::Resumed(_) => self.close_session(),
            SessionOrigin::Live => {
                self.cfg.store.archive();
            }
        }
    }

    fn close_session(&mut self) {
        let SessionOrigin::Resumed(id) = std::mem::replace(&mut self.origin, SessionOrigin::Live)
        else {
            return;
        };
        if !self.cfg.store.continue_archived_live(&id).unwrap_or(false) {
            let entries = self.cfg.store.live();
            let _ = self.cfg.store.continue_archived(&id, &entries);
        }
    }

    pub(super) fn open_resume(&mut self) {
        self.overlay = Overlay::Resume {
            catalog: Catalog::default(),
            sessions: self.cfg.store.list(),
        };
    }

    pub(super) fn resume(&mut self, id: &str) {
        self.close_session();
        let Some((id, entries)) = app::load_session(self.cfg.store.as_ref(), id) else {
            self.entries
                .push(Entry::Notice(format!("no such session: {id}")));
            return;
        };
        self.origin = SessionOrigin::Resumed(id);
        self.overflow_retried = false;
        let transcript = session::current_segment(&entries)
            .iter()
            .filter_map(|entry| match entry {
                session::Entry::Message { message } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.messages = session::context_messages(&entries);
        session::drop_incomplete_tool_calls(&mut self.messages);
        self.session_state = self.cfg.store.state();
        self.input_tokens = self.session_state.context_input;
        self.output_tokens = self.session_state.context_output;
        self.session_input = self.session_state.input;
        self.session_output = self.session_state.output;
        self.rebuild_transcript_from(&transcript);
        if let Err(error) = self
            .cfg
            .store
            .save_with_state(&entries, self.session_state.clone())
        {
            self.entries
                .push(Entry::Notice(format!("error: save session: {error}")));
        }
    }

    pub(super) fn submit(&mut self) {
        if self.input.trim().is_empty() && self.attachments.is_empty() {
            return;
        }
        if self.input.starts_with('/') {
            let command = std::mem::take(&mut self.input);
            self.cursor = 0;
            self.run_command(&command);
            return;
        }
        if self.turn.compacting() {
            self.entries
                .push(Entry::Notice("compacting; wait before sending".into()));
            return;
        }
        if matches!(self.turn, Turn::Running { .. }) && !self.attachments.is_empty() {
            self.entries.push(Entry::Notice(
                "agent is running; images attach to the next message".into(),
            ));
            return;
        }
        let images = std::mem::take(&mut self.attachments);
        let raw = std::mem::take(&mut self.input);
        let content = crate::sentinel::redact(&raw);
        self.cursor = 0;
        self.history_index = None;
        self.history.push(raw.clone());
        self.entries.push(Entry::User {
            text: raw,
            images: images.iter().map(crate::image::label).collect(),
        });
        self.transcript.invalidate();
        if let Turn::Running { steer, .. } = &self.turn {
            let _ = steer.send(content);
            return;
        }
        self.overflow_retried = false;
        self.messages.push(Message {
            role: "user".into(),
            content,
            tool_calls: Vec::new(),
            tool_call_id: Default::default(),
            reasoning: String::new(),
            images,
        });
        let entry = session::Entry::Message {
            message: self.messages.last().unwrap().clone(),
        };
        if let Err(error) = self.cfg.store.append(&[entry]) {
            self.entries
                .push(Entry::Notice(format!("error: save session: {error}")));
        }
        self.scroll = super::Scroll::Bottom;
        self.start_turn();
    }

    pub(super) fn run_command(&mut self, input: &str) {
        let argument = input.split_once(' ').map(|(_, argument)| argument.trim());
        self.transcript.invalidate();
        match app::parse_command(input) {
            Some(command) => self.execute(command, argument),
            None => self
                .entries
                .push(Entry::Notice(format!("unknown command: {input}"))),
        }
    }

    pub(super) fn execute(&mut self, command: app::Command, argument: Option<&str>) {
        if self.turn.busy()
            && matches!(
                command,
                app::Command::New
                    | app::Command::Resume
                    | app::Command::Rewind
                    | app::Command::Compact
                    | app::Command::Skill
            )
        {
            self.entries.push(Entry::Notice(
                "agent is running; ctrl+c interrupts it first".into(),
            ));
            return;
        }
        match command {
            app::Command::Quit => self.want_quit = true,
            app::Command::Help => self.overlay = Overlay::Help(Catalog::default()),
            app::Command::New => {
                self.close_session();
                self.cfg.store.archive();
                self.clear_session();
            }
            app::Command::Resume => match argument {
                Some(id) if !id.is_empty() => self.resume(id),
                _ => self.open_resume(),
            },
            app::Command::Rewind => self.open_rewind(),
            app::Command::Compact => self.start_compaction(false),
            app::Command::Copy => self.copy_last(),
            app::Command::Image => self.attach(argument),
            app::Command::Skill => match argument {
                Some(name) if !name.is_empty() => self.activate_skill(name),
                _ => self
                    .entries
                    .push(Entry::Notice("usage: /skill NAME".into())),
            },
        }
    }

    pub(super) fn activate_skill(&mut self, name: &str) {
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name).cloned() else {
            let installed = self
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let notice = if installed.is_empty() {
                "no skills installed".to_string()
            } else {
                format!("unknown skill: {name}; installed: {installed}")
            };
            self.entries.push(Entry::Notice(notice));
            return;
        };
        let body = match crate::skills::render(&skill) {
            Ok(body) => body,
            Err(error) => {
                self.entries.push(Entry::Notice(error));
                return;
            }
        };
        self.entries.push(Entry::User {
            text: format!("/skill {name}"),
            images: Vec::new(),
        });
        self.messages.push(Message {
            role: "user".into(),
            content: body,
            tool_calls: Vec::new(),
            tool_call_id: Default::default(),
            reasoning: String::new(),
            images: Vec::new(),
        });
        let entry = session::Entry::Message {
            message: self.messages.last().unwrap().clone(),
        };
        if let Err(error) = self.cfg.store.append(&[entry]) {
            self.entries
                .push(Entry::Notice(format!("error: save session: {error}")));
        }
        self.transcript.invalidate();
        self.overflow_retried = false;
        self.scroll = super::Scroll::Bottom;
        self.start_turn();
    }

    pub(super) fn attach(&mut self, argument: Option<&str>) {
        self.transcript.invalidate();
        let Some(argument) = argument.filter(|argument| !argument.is_empty()) else {
            let attached = self
                .attachments
                .iter()
                .map(crate::image::label)
                .collect::<Vec<_>>()
                .join(", ");
            let notice = if attached.is_empty() {
                "usage: /image PATH or URL, /image clear, or drop a file onto the terminal"
                    .to_string()
            } else {
                format!("attached: {attached}")
            };
            self.entries.push(Entry::Notice(notice));
            return;
        };
        if argument == "clear" {
            self.attachments.clear();
            self.entries
                .push(Entry::Notice("cleared attachments".into()));
            return;
        }
        match crate::image::attach(argument) {
            Ok(image) => {
                self.entries.push(Entry::Notice(format!(
                    "attached {}",
                    crate::image::label(&image)
                )));
                self.attachments.push(image);
            }
            Err(error) => self.entries.push(Entry::Notice(format!("error: {error}"))),
        }
    }

    pub(super) fn clear_session(&mut self) {
        self.transcript.invalidate();
        self.entries.clear();
        self.messages.clear();
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.session_input = 0;
        self.session_output = 0;
        self.session_state = SessionState::default();
        self.history.clear();
        self.history_index = None;
        self.scroll = super::Scroll::Bottom;
        self.origin = SessionOrigin::Live;
        self.overflow_retried = false;
        self.overlay = Overlay::None;
    }

    pub(super) fn rewind_to(&mut self, index: usize) {
        let entries = app::rewind_entries(&self.messages, index);
        if let Err(error) = self
            .cfg
            .store
            .save_with_state(&entries, SessionState::default())
        {
            self.entries
                .push(Entry::Notice(format!("error: save session: {error}")));
            return;
        }
        self.messages = session::context_messages(&entries);
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.session_input = 0;
        self.session_output = 0;
        self.session_state = SessionState::default();
        self.overflow_retried = false;
        self.rebuild_transcript();
        self.entries.push(Entry::Notice(format!(
            "rewound · {} messages remaining",
            self.messages.len()
        )));
    }

    pub(super) fn copy_last(&mut self) {
        let Some(text) =
            app::last_assistant_response(&self.messages).or_else(|| self.last_assistant_source())
        else {
            self.entries
                .push(Entry::Notice("no assistant response to copy".into()));
            return;
        };
        super::copy_to_clipboard(text);
        self.entries
            .push(Entry::Notice("copied last response".into()));
    }

    pub(super) fn start_compaction(&mut self, resume: bool) {
        if self.turn.compacting() {
            self.entries
                .push(Entry::Notice("already compacting".into()));
            return;
        }
        if self.turn.busy() {
            self.entries.push(Entry::Notice("agent is running".into()));
            return;
        }
        if self.messages.len() < 4 {
            self.entries
                .push(Entry::Notice("session too small to compact".into()));
            return;
        }
        let provider = self.provider();
        let model = self.cfg.model.clone();
        let entries = self.cfg.store.live();
        let state = self.cfg.store.state();
        let (sender, receiver) = mpsc::channel();
        self.turn = Turn::Compacting {
            events: receiver,
            resume,
        };
        self.turn_started = Instant::now();
        status::report(State::Working);
        std::thread::spawn(move || {
            let result = session::compact_with_state(
                &provider,
                &model,
                &entries,
                &state,
                session::ContextOptions::default(),
            )
            .map(|(summary, tokens_before, retained)| (summary, tokens_before, retained, entries));
            let _ = sender.send(TurnEvent::Compacted(result));
        });
    }

    pub(super) fn start_turn(&mut self) {
        if self.turn.busy() {
            return;
        }
        let messages = std::mem::take(&mut self.messages);
        let base = messages.len();
        let provider = self.provider();
        let model = self.cfg.model.clone();
        let system = self.cfg.system.clone();
        let tools = self.tools();
        let threshold = self
            .cfg
            .spec
            .context_window
            .map(|window| window.saturating_sub(16384));
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let (steer, steer_receiver) = mpsc::channel();
        self.turn = Turn::Running {
            events: receiver,
            steer,
            base,
        };
        self.turn_started = Instant::now();
        status::report(State::Working);
        std::thread::spawn(move || {
            let end = {
                let mut observer = RatatuiObserver { sender: &sender };
                let mut control = TuiControl {
                    steer: steer_receiver,
                    threshold,
                };
                run::run_stream_owned(
                    &provider,
                    &RunOptions {
                        model: &model,
                        system: &system,
                        tools: &tools,
                        max_turns: usize::MAX,
                    },
                    messages,
                    &cancel,
                    &mut observer,
                    &mut control,
                )
            };
            let _ = sender.send(TurnEvent::End {
                messages: end.messages,
                usage: end.usage,
                context: end.context,
                outcome: end.outcome,
            });
        });
    }
}
