use crate::Message;
use crate::Usage;
use crate::app;
use crate::run::{AgentEvent, Control, Observer, Outcome};
use crate::session;
use std::sync::mpsc::{Receiver, Sender};

pub(super) enum TurnEvent {
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
        outcome: Outcome,
    },
}

pub(super) struct RatatuiObserver<'a> {
    pub(super) sender: &'a Sender<TurnEvent>,
}

impl Observer for RatatuiObserver<'_> {
    fn event(&mut self, event: AgentEvent<'_>) {
        match event {
            AgentEvent::AssistantDelta(text) => {
                let _ = self.sender.send(TurnEvent::AssistantDelta(text.into()));
            }
            AgentEvent::ToolStart(call) => {
                let _ = self
                    .sender
                    .send(TurnEvent::ToolStart(app::tool_label(call, true)));
            }
            AgentEvent::ToolDelta { text, .. } => {
                let _ = self.sender.send(TurnEvent::ToolDelta(text.into()));
            }
            AgentEvent::ToolResult { call, .. } => {
                let _ = self
                    .sender
                    .send(TurnEvent::ToolResult(app::tool_label(call, false)));
            }
            AgentEvent::Tokens {
                input,
                output,
                cached_input,
            } => {
                let _ = self.sender.send(TurnEvent::Tokens(Usage {
                    input,
                    output,
                    cached_input,
                    reasoning: 0,
                }));
            }
            AgentEvent::AssistantDone | AgentEvent::Assistant { .. } | AgentEvent::Tool { .. } => {}
        }
    }
}

pub(super) struct TuiControl {
    pub(super) steer: Receiver<String>,
    pub(super) threshold: Option<usize>,
}

impl Control for TuiControl {
    fn pending_user_input(&mut self) -> Option<String> {
        self.steer.try_recv().ok()
    }

    fn should_compact(&mut self, input: usize, output: usize) -> bool {
        self.threshold
            .is_some_and(|threshold| input.saturating_add(output) > threshold)
    }
}
