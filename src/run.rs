use crate::sentinel;
use crate::{
    Error, Message, Provider, Request, Response, StreamEvent, Tool, ToolCall, ToolOutput, Usage,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_RETRIES: usize = 2;

fn retryable(e: &Error) -> bool {
    match e {
        Error::Http { status, .. } => {
            *status == 408 || *status == 409 || *status == 429 || *status >= 500
        }
        Error::Transport(_) => true,
        _ => false,
    }
}

fn backoff(attempt: usize) -> u64 {
    let base = (500u64 << (attempt - 1).min(4)).min(8000);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    base - now % (base / 4 + 1)
}

fn cancelled(cancel: &Arc<AtomicBool>) -> bool {
    if cancel.load(Ordering::Relaxed) {
        crate::tools::kill_children();
        true
    } else {
        false
    }
}

fn sleep_with_cancel(ms: u64, cancel: &Arc<AtomicBool>) -> Result<(), Error> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if cancelled(cancel) {
            return Err(Error::Provider("interrupted".into()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[derive(Debug)]
pub enum AgentEvent<'a> {
    AssistantDelta(&'a str),
    AssistantDone,
    ToolStart(&'a ToolCall),
    ToolDelta {
        call: &'a ToolCall,
        text: &'a str,
    },
    ToolResult {
        call: &'a ToolCall,
        output: &'a ToolOutput,
        elapsed: Duration,
    },
    Tokens {
        input: usize,
        output: usize,
        cached_input: usize,
    },
    Assistant {
        turn: usize,
        message: &'a Message,
        usage: Usage,
    },
    Tool {
        turn: usize,
        message: &'a Message,
    },
}

pub trait Observer {
    fn event(&mut self, event: AgentEvent<'_>);
}

pub trait Control {
    fn pending_user_input(&mut self) -> Option<String> {
        None
    }

    fn should_compact(&mut self, _input: usize, _output: usize) -> bool {
        false
    }
}

pub trait RuntimeHooks: Observer + Control {}
impl<T: Observer + Control + ?Sized> RuntimeHooks for T {}

struct HooksPair<'a> {
    observer: &'a mut dyn Observer,
    control: &'a mut dyn Control,
}

impl Observer for HooksPair<'_> {
    fn event(&mut self, event: AgentEvent<'_>) {
        self.observer.event(event);
    }
}

impl Control for HooksPair<'_> {
    fn pending_user_input(&mut self) -> Option<String> {
        self.control.pending_user_input()
    }

    fn should_compact(&mut self, input: usize, output: usize) -> bool {
        self.control.should_compact(input, output)
    }
}

pub struct RunOptions<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub tools: &'a [Tool],
    pub max_turns: usize,
}

#[derive(Debug)]
pub enum Outcome {
    Done,
    MaxTurns,
    Cancelled,
    Compact,
    Failed(String),
}
pub struct RunEnd {
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub context: Usage,
    pub outcome: Outcome,
}

pub fn run_stream_with<P: Provider>(
    provider: &P,
    opts: &RunOptions,
    msgs: &[Message],
    cancel: &Arc<AtomicBool>,
    observer: &mut dyn Observer,
    control: &mut dyn Control,
) -> RunEnd {
    let messages = msgs.iter().map(sentinel::redact_message).collect();
    let mut hooks = HooksPair { observer, control };
    run_inner(provider, opts, messages, cancel, &mut hooks)
}

/// Run with messages that the caller already redacted.
pub fn run_stream_owned<P: Provider>(
    provider: &P,
    opts: &RunOptions,
    messages: Vec<Message>,
    cancel: &Arc<AtomicBool>,
    observer: &mut dyn Observer,
    control: &mut dyn Control,
) -> RunEnd {
    let mut hooks = HooksPair { observer, control };
    run_inner(provider, opts, messages, cancel, &mut hooks)
}

fn run_inner<P: Provider>(
    provider: &P,
    opts: &RunOptions,
    mut h: Vec<Message>,
    cancel: &Arc<AtomicBool>,
    hooks: &mut dyn RuntimeHooks,
) -> RunEnd {
    let started = Instant::now();
    let mut usage = Usage::default();
    let mut context_usage = Usage::default();
    let tool_context = crate::ToolContext {
        cancel: Some(cancel.clone()),
    };
    trace(format!(
        "run start messages={} system_bytes={} tools={}",
        h.len(),
        opts.system.len(),
        opts.tools.len()
    ));
    for turn in 0..opts.max_turns {
        trace(format!("turn={} start messages={}", turn + 1, h.len()));
        if cancelled(cancel) {
            return RunEnd {
                messages: h,
                usage,
                context: context_usage,
                outcome: Outcome::Cancelled,
            };
        }
        if let Some(text) = hooks.pending_user_input() {
            h.push(user_message(text));
        }
        let (resp, calls) = match stream(provider, opts, &h, cancel, hooks) {
            Ok(x) => x,
            Err(e) => {
                if cancelled(cancel) {
                    return RunEnd {
                        messages: h,
                        usage,
                        context: context_usage,
                        outcome: Outcome::Cancelled,
                    };
                }
                return RunEnd {
                    messages: h,
                    usage,
                    context: context_usage,
                    outcome: Outcome::Failed(e),
                };
            }
        };
        trace(format!(
            "turn={} model_done elapsed_ms={} input_tokens={} cached_input_tokens={} output_tokens={} tool_calls={}",
            turn + 1,
            started.elapsed().as_millis(),
            resp.usage.input,
            resp.usage.cached_input,
            resp.usage.output,
            calls.len()
        ));
        usage = Usage {
            input: usage.input + resp.usage.input,
            output: usage.output + resp.usage.output,
            cached_input: usage.cached_input + resp.usage.cached_input,
            reasoning: usage.reasoning + resp.usage.reasoning,
        };
        context_usage = resp.usage;
        h.push(resp.message);
        hooks.event(AgentEvent::Assistant {
            turn,
            message: h.last().unwrap(),
            usage: resp.usage,
        });
        hooks.event(AgentEvent::AssistantDone);
        if calls.is_empty() {
            if let Some(text) = hooks.pending_user_input() {
                h.push(user_message(text));
                continue;
            }
            trace(format!(
                "run done elapsed_ms={} turns={} input_tokens={} cached_input_tokens={} output_tokens={}",
                started.elapsed().as_millis(),
                turn + 1,
                usage.input,
                usage.cached_input,
                usage.output
            ));
            return RunEnd {
                messages: h,
                usage,
                context: context_usage,
                outcome: Outcome::Done,
            };
        }
        let truncated = resp.stop_reason.is_length();
        if !run_tool_batch(
            opts.tools,
            calls,
            truncated,
            turn,
            cancel,
            &tool_context,
            hooks,
            &mut h,
        ) {
            return RunEnd {
                messages: h,
                usage,
                context: context_usage,
                outcome: Outcome::Cancelled,
            };
        }
        if hooks.should_compact(resp.usage.input, resp.usage.output) {
            if let Some(text) = hooks.pending_user_input() {
                h.push(user_message(text));
            }
            return RunEnd {
                messages: h,
                usage,
                context: context_usage,
                outcome: Outcome::Compact,
            };
        }
    }
    RunEnd {
        messages: h,
        usage,
        context: context_usage,
        outcome: Outcome::MaxTurns,
    }
}

fn user_message(text: String) -> Message {
    Message {
        role: "user".into(),
        content: sentinel::redact(&text),
        tool_calls: Vec::new(),
        tool_call_id: Default::default(),
        reasoning: String::new(),
        images: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tool_batch(
    tools: &[Tool],
    calls: Vec<ToolCall>,
    truncated: bool,
    turn: usize,
    cancel: &Arc<AtomicBool>,
    context: &crate::ToolContext,
    hooks: &mut dyn RuntimeHooks,
    h: &mut Vec<Message>,
) -> bool {
    let any_sequential = calls.iter().any(|c| {
        tools
            .iter()
            .find(|t| t.name == c.name.as_str())
            .map(|t| t.sequential)
            .unwrap_or(false)
    });
    if truncated || any_sequential || calls.len() <= 1 {
        let mut interrupted = false;
        for call in calls {
            interrupted |= cancelled(cancel);
            hooks.event(AgentEvent::ToolStart(&call));
            let (output, elapsed) = if interrupted {
                (
                    ToolOutput::text("error: tool call not executed: the run was interrupted."),
                    Duration::ZERO,
                )
            } else if truncated {
                (
                    ToolOutput::text(
                        "error: tool call not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    ),
                    Duration::ZERO,
                )
            } else {
                exec(tools, &call, context, hooks)
            };
            hooks.event(AgentEvent::ToolResult {
                call: &call,
                output: &output,
                elapsed,
            });
            push_tool_result(h, call, output);
            hooks.event(AgentEvent::Tool {
                turn,
                message: h.last().unwrap(),
            });
        }
        !cancelled(cancel)
    } else {
        run_parallel(tools, calls, turn, cancel, context, hooks, h)
    }
}

fn push_tool_result(h: &mut Vec<Message>, call: ToolCall, output: ToolOutput) {
    h.push(Message {
        role: "tool".into(),
        content: sentinel::redact(&output.text),
        tool_calls: Vec::new(),
        tool_call_id: call.id,
        reasoning: String::new(),
        images: output.images,
    });
}

enum ParallelMsg {
    Delta {
        idx: usize,
        text: String,
    },
    Done {
        idx: usize,
        output: ToolOutput,
        elapsed: Duration,
    },
}

#[allow(clippy::too_many_arguments)]
fn run_parallel(
    tools: &[Tool],
    calls: Vec<ToolCall>,
    turn: usize,
    cancel: &Arc<AtomicBool>,
    context: &crate::ToolContext,
    hooks: &mut dyn RuntimeHooks,
    h: &mut Vec<Message>,
) -> bool {
    for call in &calls {
        hooks.event(AgentEvent::ToolStart(call));
    }
    let (ptx, prx) = std::sync::mpsc::channel::<ParallelMsg>();
    let mut outputs: Vec<Option<(ToolOutput, Duration)>> = (0..calls.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        for (idx, call) in calls.iter().enumerate() {
            let ptx = ptx.clone();
            let cancel = cancel.clone();
            let context = context.clone();
            scope.spawn(move || {
                let (output, elapsed) = if cancelled(&cancel) {
                    (
                        ToolOutput::text("error: tool call not executed: the run was interrupted."),
                        Duration::ZERO,
                    )
                } else {
                    run_tool(tools, call, &context, &mut |text| {
                        let _ = ptx.send(ParallelMsg::Delta {
                            idx,
                            text: text.to_string(),
                        });
                    })
                };
                let _ = ptx.send(ParallelMsg::Done {
                    idx,
                    output,
                    elapsed,
                });
            });
        }
        drop(ptx);
        for msg in prx.iter() {
            match msg {
                ParallelMsg::Delta { idx, text } => {
                    if let Some(call) = calls.get(idx) {
                        hooks.event(AgentEvent::ToolDelta { call, text: &text });
                    }
                }
                ParallelMsg::Done {
                    idx,
                    output,
                    elapsed,
                } => outputs[idx] = Some((output, elapsed)),
            }
        }
    });
    for (idx, call) in calls.iter().enumerate() {
        let (content, elapsed) = outputs[idx]
            .take()
            .expect("scoped tool thread always reports a result");
        hooks.event(AgentEvent::ToolResult {
            call,
            output: &content,
            elapsed,
        });
        push_tool_result(h, call.clone(), content);
        hooks.event(AgentEvent::Tool {
            turn,
            message: h.last().unwrap(),
        });
    }
    !cancelled(cancel)
}

fn run_tool(
    tools: &[Tool],
    call: &ToolCall,
    context: &crate::ToolContext,
    progress: &mut dyn FnMut(&str),
) -> (ToolOutput, Duration) {
    let started = Instant::now();
    for t in tools {
        if t.name == call.name.as_str() {
            let output = match (t.run)(&call.arguments, context, progress) {
                Ok(output) => output,
                Err(error) => output_from_error(error),
            };
            let elapsed = started.elapsed();
            trace(format!(
                "tool name={} elapsed_ms={} argument_bytes={} output_bytes={}",
                call.name,
                elapsed.as_millis(),
                call.arguments.len(),
                output.text.len(),
            ));
            trace(format!("tool error={}", output.error));
            return (output, elapsed);
        }
    }
    let elapsed = started.elapsed();
    trace(format!(
        "tool name={} elapsed_ms={} unknown=true",
        call.name,
        elapsed.as_millis()
    ));
    (
        ToolOutput::error(format!("error: unknown tool: {}", call.name)),
        elapsed,
    )
}

fn stream<P: Provider>(
    provider: &P,
    opts: &RunOptions,
    h: &[Message],
    cancel: &Arc<AtomicBool>,
    hooks: &mut dyn RuntimeHooks,
) -> Result<(Response, Vec<ToolCall>), String> {
    let req = Request {
        model: opts.model,
        system: opts.system,
        messages: h,
        tools: opts.tools,
    };
    let mut attempt = 0;
    loop {
        let started = Instant::now();
        let handle = provider.stream(&req, cancel);
        let mut calls = Vec::new();
        let mut committed = false;
        let mut first_event = false;
        while let Ok(ev) = handle.events().recv() {
            match ev {
                StreamEvent::Content(d) => {
                    if !first_event {
                        trace(format!(
                            "model first_event_ms={}",
                            started.elapsed().as_millis()
                        ));
                        first_event = true;
                    }
                    committed = true;
                    hooks.event(AgentEvent::AssistantDelta(&d));
                }
                StreamEvent::ToolCall(c) => {
                    if !first_event {
                        trace(format!(
                            "model first_event_ms={}",
                            started.elapsed().as_millis()
                        ));
                        first_event = true;
                    }
                    committed = true;
                    calls.push(c);
                }
                StreamEvent::Tokens {
                    input,
                    output,
                    cached_input,
                } => {
                    hooks.event(AgentEvent::Tokens {
                        input,
                        output,
                        cached_input,
                    });
                }
                StreamEvent::Done => break,
            }
        }
        match handle.join() {
            Ok(resp) if !resp.stop_reason.is_interrupted() => {
                trace(format!(
                    "model request_done_ms={} attempt={}",
                    started.elapsed().as_millis(),
                    attempt + 1
                ));
                return Ok((resp, calls));
            }
            Ok(resp) => {
                if attempt >= MAX_RETRIES || committed {
                    return Err(format!("provider interrupted: {}", resp.stop_reason));
                }
            }
            Err(e) => {
                if attempt >= MAX_RETRIES || committed || !retryable(&e) {
                    return Err(e.to_string());
                }
            }
        }
        attempt += 1;
        let delay = backoff(attempt);
        trace(format!("model retry={} backoff_ms={}", attempt, delay));
        sleep_with_cancel(delay, cancel).map_err(|e| e.to_string())?;
    }
}

fn exec(
    tools: &[Tool],
    call: &ToolCall,
    context: &crate::ToolContext,
    hooks: &mut dyn RuntimeHooks,
) -> (ToolOutput, Duration) {
    run_tool(tools, call, context, &mut |text| {
        hooks.event(AgentEvent::ToolDelta { call, text })
    })
}

fn output_from_error(error: crate::ToolError) -> ToolOutput {
    ToolOutput::from(error)
}

fn trace(message: String) {
    if std::env::var_os("AXE_TRACE").is_some() {
        eprintln!("trace: {message}");
    }
}

#[cfg(test)]
mod tests {
    use crate::StopReason;

    #[test]
    fn the_provider_interruptions_are_retryable() {
        assert!(StopReason::from("insufficient_system_resource").is_interrupted());
        assert!(StopReason::from("aborted").is_interrupted());
        assert!(!StopReason::from("stop").is_interrupted());
        assert!(!StopReason::from("length").is_interrupted());
        assert!(!StopReason::from("tool_calls").is_interrupted());
        assert!(!StopReason::from("content_filter").is_interrupted());
    }
}
