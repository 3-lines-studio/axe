//! Streaming agent loop shared by the SDK and the TUI.
//!
//! The loop is the only logic: messages -> LLM -> tool calls -> results ->
//! repeat. It never mutates its input; the transcript it builds is
//! append-only. Compaction and steering live outside this module.

use crate::{Error, Message, Provider, Request, Response, StreamEvent, Tool, ToolCall, Usage};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Retries for retryable provider failures (rate limits, 5xx, transport
/// errors) before the request reaches the turn loop.
const MAX_RETRIES: usize = 2;

fn retryable(e: &Error) -> bool {
    match e {
        Error::Http { status, .. } => {
            *status == 408 || *status == 409 || *status == 429 || *status >= 500
        }
        // Connection-level failures are worth another attempt; provider
        // errors (bad responses, parse failures, cancellation) are not.
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

fn sleep_with_cancel(ms: u64, cancel: &Arc<AtomicBool>) -> Result<(), Error> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Provider("interrupted".into()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

pub trait Sink {
    fn assistant_delta(&mut self, _text: &str) {}
    fn assistant_done(&mut self) {}
    fn tool_start(&mut self, _call: &ToolCall) {}
    fn tool_delta(&mut self, _call: &ToolCall, _text: &str) {}
    fn tool_result(&mut self, _call: &ToolCall) {}
    fn tokens(&mut self, _input: usize, _output: usize, _cached_input: usize) {}
    fn should_compact(&mut self, _input: usize, _output: usize) -> bool {
        false
    }
    fn assistant(&mut self, _turn: usize, _msg: &Message, _usage: Usage) {}
    fn tool(&mut self, _turn: usize, _msg: &Message) {}
    /// Poll for a user message typed while the agent was running.
    fn pending_user_input(&mut self) -> Option<String> {
        None
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
    pub outcome: Outcome,
}

pub fn run_stream<P: Provider>(
    provider: &P,
    opts: &RunOptions,
    msgs: &[Message],
    cancel: &Arc<AtomicBool>,
    sink: &mut dyn Sink,
) -> RunEnd {
    let mut h = msgs.to_vec();
    let mut usage = Usage::default();
    for turn in 0..opts.max_turns {
        if cancel.load(Ordering::Relaxed) {
            return RunEnd {
                messages: h,
                usage,
                outcome: Outcome::Cancelled,
            };
        }
        if let Some(text) = sink.pending_user_input() {
            h.push(user_message(text));
        }
        let (resp, calls) = match stream(provider, opts, &h, cancel, sink) {
            Ok(x) => x,
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    return RunEnd {
                        messages: h,
                        usage,
                        outcome: Outcome::Cancelled,
                    };
                }
                return RunEnd {
                    messages: h,
                    usage,
                    outcome: Outcome::Failed(e),
                };
            }
        };
        usage = Usage {
            input: usage.input + resp.usage.input,
            output: usage.output + resp.usage.output,
            cached_input: usage.cached_input + resp.usage.cached_input,
        };
        h.push(resp.message);
        sink.assistant(turn, h.last().unwrap(), resp.usage);
        sink.assistant_done();
        if calls.is_empty() {
            if let Some(text) = sink.pending_user_input() {
                h.push(user_message(text));
                continue;
            }
            return RunEnd {
                messages: h,
                usage,
                outcome: Outcome::Done,
            };
        }
        // A "length" stop means the output was cut off by the token limit, so
        // every tool call in the message may carry truncated arguments. Fail
        // them all instead of executing potentially borked calls.
        let truncated = resp.stop_reason == "length";
        if !run_tool_batch(opts.tools, calls, truncated, turn, cancel, sink, &mut h) {
            return RunEnd {
                messages: h,
                usage,
                outcome: Outcome::Cancelled,
            };
        }
        if sink.should_compact(resp.usage.input, resp.usage.output) {
            if let Some(text) = sink.pending_user_input() {
                h.push(user_message(text));
            }
            return RunEnd {
                messages: h,
                usage,
                outcome: Outcome::Compact,
            };
        }
    }
    RunEnd {
        messages: h,
        usage,
        outcome: Outcome::MaxTurns,
    }
}

fn user_message(text: String) -> Message {
    Message {
        role: "user".into(),
        content: text,
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    }
}

/// Execute one assistant message's tool calls, appending tool results to `h`.
/// Returns false when the run should stop due to cancellation.
fn run_tool_batch(
    tools: &[Tool],
    calls: Vec<ToolCall>,
    truncated: bool,
    turn: usize,
    cancel: &Arc<AtomicBool>,
    sink: &mut dyn Sink,
    h: &mut Vec<Message>,
) -> bool {
    let any_sequential = calls.iter().any(|c| {
        tools
            .iter()
            .find(|t| t.name == c.name)
            .map(|t| t.sequential)
            .unwrap_or(false)
    });
    if truncated || any_sequential || calls.len() <= 1 {
        let mut interrupted = false;
        for call in calls {
            interrupted |= cancel.load(Ordering::Relaxed);
            sink.tool_start(&call);
            let output = if interrupted {
                // Synthesize a result for every un-executed call so the
                // transcript stays valid: providers reject an assistant
                // message whose tool_calls lack matching tool results.
                "error: tool call not executed: the run was interrupted.".to_string()
            } else if truncated {
                "error: tool call not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.".to_string()
            } else {
                exec(tools, &call, sink)
            };
            sink.tool_result(&call);
            push_tool_result(h, call, output);
            sink.tool(turn, h.last().unwrap());
        }
        !cancel.load(Ordering::Relaxed)
    } else {
        run_parallel(tools, calls, turn, cancel, sink, h)
    }
}

fn push_tool_result(h: &mut Vec<Message>, call: ToolCall, content: String) {
    h.push(Message {
        role: "tool".into(),
        content,
        tool_calls: Vec::new(),
        tool_call_id: call.id,
    });
}

enum ParallelMsg {
    Delta { idx: usize, text: String },
    Done { idx: usize, output: String },
}

fn run_parallel(
    tools: &[Tool],
    calls: Vec<ToolCall>,
    turn: usize,
    cancel: &Arc<AtomicBool>,
    sink: &mut dyn Sink,
    h: &mut Vec<Message>,
) -> bool {
    for call in &calls {
        sink.tool_start(call);
    }
    let (ptx, prx) = std::sync::mpsc::channel::<ParallelMsg>();
    let mut outputs: Vec<Option<String>> = vec![None; calls.len()];
    std::thread::scope(|scope| {
        for (idx, call) in calls.iter().enumerate() {
            let ptx = ptx.clone();
            let cancel = cancel.clone();
            scope.spawn(move || {
                let output = if cancel.load(Ordering::Relaxed) {
                    // Synthesize a result so the transcript stays valid.
                    "error: tool call not executed: the run was interrupted.".to_string()
                } else {
                    run_tool(tools, call, &mut |text| {
                        let _ = ptx.send(ParallelMsg::Delta {
                            idx,
                            text: text.to_string(),
                        });
                    })
                };
                let _ = ptx.send(ParallelMsg::Done { idx, output });
            });
        }
        drop(ptx);
        for msg in prx.iter() {
            match msg {
                ParallelMsg::Delta { idx, text } => {
                    if let Some(call) = calls.get(idx) {
                        sink.tool_delta(call, &text);
                    }
                }
                ParallelMsg::Done { idx, output } => outputs[idx] = Some(output),
            }
        }
    });
    for (idx, call) in calls.iter().enumerate() {
        sink.tool_result(call);
        let content = outputs[idx]
            .clone()
            .unwrap_or_else(|| "error: tool thread panicked".into());
        push_tool_result(h, call.clone(), content);
        sink.tool(turn, h.last().unwrap());
    }
    !cancel.load(Ordering::Relaxed)
}

fn run_tool(tools: &[Tool], call: &ToolCall, progress: &mut dyn FnMut(&str)) -> String {
    for t in tools {
        if t.name == call.name {
            return (t.run)(&call.arguments, progress);
        }
    }
    format!("error: unknown tool: {}", call.name)
}

fn stream<P: Provider>(
    provider: &P,
    opts: &RunOptions,
    h: &[Message],
    cancel: &Arc<AtomicBool>,
    sink: &mut dyn Sink,
) -> Result<(Response, Vec<ToolCall>), String> {
    let req = Request {
        model: opts.model,
        system: opts.system,
        messages: h,
        tools: opts.tools,
    };
    let mut attempt = 0;
    loop {
        let handle = provider.stream(&req, cancel);
        let mut calls = Vec::new();
        let mut forwarded = 0usize;
        while let Ok(ev) = handle.events().recv() {
            match ev {
                StreamEvent::Content(d) => {
                    forwarded += 1;
                    sink.assistant_delta(&d);
                }
                StreamEvent::ToolCall(c) => {
                    forwarded += 1;
                    calls.push(c);
                }
                StreamEvent::Tokens {
                    input,
                    output,
                    cached_input,
                } => {
                    sink.tokens(input, output, cached_input);
                }
                StreamEvent::Done => break,
            }
        }
        match handle.join() {
            Ok(resp) => return Ok((resp, calls)),
            Err(e) => {
                // Only retry failures that emitted no events yet: once content
                // reached the sink, a re-run would duplicate it.
                if attempt >= MAX_RETRIES || forwarded > 0 || !retryable(&e) {
                    return Err(e.to_string());
                }
                attempt += 1;
                sleep_with_cancel(backoff(attempt), cancel).map_err(|e| e.to_string())?;
            }
        }
    }
}

fn exec(tools: &[Tool], call: &ToolCall, sink: &mut dyn Sink) -> String {
    run_tool(tools, call, &mut |text| sink.tool_delta(call, text))
}
