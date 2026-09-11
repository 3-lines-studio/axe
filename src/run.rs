//! The loop is the only logic: messages -> LLM -> tool calls -> results ->
//! repeat. It never mutates its input; the transcript it builds is
//! append-only. Compaction and steering live outside this module.

use crate::{
    Error, Message, Provider, Request, Response, StreamEvent, Tool, ToolCall, ToolOutput, Usage,
};
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
    /// Tokens billed across every request in the run.
    pub usage: Usage,
    /// Usage of the last request: its input is the context size the provider
    /// saw, its output the tokens generated in the final turn. Callers
    /// persist this for the compaction budget and the resume display.
    pub context: Usage,
    pub outcome: Outcome,
}

pub fn run_stream<P: Provider>(
    provider: &P,
    opts: &RunOptions,
    msgs: &[Message],
    cancel: &Arc<AtomicBool>,
    sink: &mut dyn Sink,
) -> RunEnd {
    let started = Instant::now();
    let mut h = msgs.to_vec();
    let mut usage = Usage::default();
    let mut context = Usage::default();
    trace(format!(
        "run start messages={} system_bytes={} tools={}",
        msgs.len(),
        opts.system.len(),
        opts.tools.len()
    ));
    for turn in 0..opts.max_turns {
        trace(format!("turn={} start messages={}", turn + 1, h.len()));
        if cancelled(cancel) {
            return RunEnd {
                messages: h,
                usage,
                context,
                outcome: Outcome::Cancelled,
            };
        }
        if let Some(text) = sink.pending_user_input() {
            h.push(user_message(text));
        }
        let (resp, calls) = match stream(provider, opts, &h, cancel, sink) {
            Ok(x) => x,
            Err(e) => {
                if cancelled(cancel) {
                    return RunEnd {
                        messages: h,
                        usage,
                        context,
                        outcome: Outcome::Cancelled,
                    };
                }
                return RunEnd {
                    messages: h,
                    usage,
                    context,
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
        };
        context = resp.usage;
        h.push(resp.message);
        sink.assistant(turn, h.last().unwrap(), resp.usage);
        sink.assistant_done();
        if calls.is_empty() {
            if let Some(text) = sink.pending_user_input() {
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
                context,
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
                context,
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
                context,
                outcome: Outcome::Compact,
            };
        }
    }
    RunEnd {
        messages: h,
        usage,
        context,
        outcome: Outcome::MaxTurns,
    }
}

fn user_message(text: String) -> Message {
    Message {
        role: "user".into(),
        content: text,
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
        reasoning: String::new(),
        images: Vec::new(),
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
            interrupted |= cancelled(cancel);
            sink.tool_start(&call);
            let output = if interrupted {
                // Synthesize a result for every un-executed call so the
                // transcript stays valid: providers reject an assistant
                // message whose tool_calls lack matching tool results.
                ToolOutput::text("error: tool call not executed: the run was interrupted.")
            } else if truncated {
                ToolOutput::text(
                    "error: tool call not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                )
            } else {
                exec(tools, &call, sink)
            };
            sink.tool_result(&call);
            push_tool_result(h, call, output);
            sink.tool(turn, h.last().unwrap());
        }
        !cancelled(cancel)
    } else {
        run_parallel(tools, calls, turn, cancel, sink, h)
    }
}

fn push_tool_result(h: &mut Vec<Message>, call: ToolCall, output: ToolOutput) {
    h.push(Message {
        role: "tool".into(),
        content: output.text,
        tool_calls: Vec::new(),
        tool_call_id: call.id,
        reasoning: String::new(),
        images: output.images,
    });
}

enum ParallelMsg {
    Delta { idx: usize, text: String },
    Done { idx: usize, output: ToolOutput },
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
    let mut outputs: Vec<Option<ToolOutput>> = (0..calls.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        for (idx, call) in calls.iter().enumerate() {
            let ptx = ptx.clone();
            let cancel = cancel.clone();
            scope.spawn(move || {
                let output = if cancelled(&cancel) {
                    ToolOutput::text("error: tool call not executed: the run was interrupted.")
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
        // Every spawned thread sends exactly one Done before the scope joins,
        // so a missing result means the thread panicked. That aborts the
        // process (release, panic = "immediate-abort") or is re-raised by
        // thread::scope (test profile) before this loop runs, so the result is
        // always present.
        let content = outputs[idx]
            .take()
            .expect("scoped tool thread always reports a result");
        push_tool_result(h, call.clone(), content);
        sink.tool(turn, h.last().unwrap());
    }
    !cancelled(cancel)
}

fn run_tool(tools: &[Tool], call: &ToolCall, progress: &mut dyn FnMut(&str)) -> ToolOutput {
    let started = Instant::now();
    for t in tools {
        if t.name == call.name {
            let output = (t.run)(&call.arguments, progress);
            trace(format!(
                "tool name={} elapsed_ms={} argument_bytes={} output_bytes={}",
                call.name,
                started.elapsed().as_millis(),
                call.arguments.len(),
                output.text.len()
            ));
            return output;
        }
    }
    trace(format!(
        "tool name={} elapsed_ms={} unknown=true",
        call.name,
        started.elapsed().as_millis()
    ));
    ToolOutput::text(format!("error: unknown tool: {}", call.name))
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
        let started = Instant::now();
        let handle = provider.stream(&req, cancel);
        let mut calls = Vec::new();
        let mut forwarded = 0usize;
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
                    forwarded += 1;
                    sink.assistant_delta(&d);
                }
                StreamEvent::ToolCall(c) => {
                    if !first_event {
                        trace(format!(
                            "model first_event_ms={}",
                            started.elapsed().as_millis()
                        ));
                        first_event = true;
                    }
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
            Ok(resp) => {
                trace(format!(
                    "model request_done_ms={} attempt={}",
                    started.elapsed().as_millis(),
                    attempt + 1
                ));
                return Ok((resp, calls));
            }
            Err(e) => {
                // Only retry failures that emitted no events yet: once content
                // reached the sink, a re-run would duplicate it.
                if attempt >= MAX_RETRIES || forwarded > 0 || !retryable(&e) {
                    return Err(e.to_string());
                }
                attempt += 1;
                let delay = backoff(attempt);
                trace(format!(
                    "model retry={} backoff_ms={} error={}",
                    attempt, delay, e
                ));
                sleep_with_cancel(delay, cancel).map_err(|e| e.to_string())?;
            }
        }
    }
}

fn exec(tools: &[Tool], call: &ToolCall, sink: &mut dyn Sink) -> ToolOutput {
    run_tool(tools, call, &mut |text| sink.tool_delta(call, text))
}

fn trace(message: String) {
    if std::env::var_os("AXE_TRACE").is_some() {
        eprintln!("trace: {message}");
    }
}
