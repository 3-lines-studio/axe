//! Agent loop against a fake provider.

use axe::{
    Agent, Error, Message, Provider, Request, Response, StreamEvent, StreamHandle, ToolCall, Usage,
    new_tool,
};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

#[derive(Default)]
struct Fake {
    responses: RefCell<VecDeque<Response>>,
    requests: RefCell<Vec<Vec<Message>>>,
}

impl Provider for Fake {
    fn complete(&self, req: &Request) -> Result<Response, Error> {
        self.requests.borrow_mut().push(req.messages.to_vec());
        Ok(self
            .responses
            .borrow_mut()
            .pop_front()
            .expect("no fake response"))
    }
    fn stream(&self, req: &Request, _cancel: &Arc<AtomicBool>) -> StreamHandle {
        let (tx, rx) = mpsc::channel();
        let resp = self.complete(req).expect("no fake response");
        let thread = std::thread::spawn(move || {
            for c in &resp.message.tool_calls {
                let _ = tx.send(StreamEvent::ToolCall(c.clone()));
            }
            if !resp.message.content.is_empty() {
                let _ = tx.send(StreamEvent::Content(resp.message.content.clone()));
            }
            let _ = tx.send(StreamEvent::Tokens {
                input: resp.usage.input,
                output: resp.usage.output,
                cached_input: resp.usage.cached_input,
            });
            let _ = tx.send(StreamEvent::Done);
            Ok(resp)
        });
        StreamHandle::new(rx, thread)
    }
}

fn call_tool(id: &str, name: &str, args: &str) -> Message {
    Message {
        role: "assistant".into(),
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }],
        tool_call_id: String::new(),
    }
}

fn user(content: &str) -> Message {
    Message {
        role: "user".into(),
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    }
}

fn assistant(content: &str) -> Message {
    Message {
        role: "assistant".into(),
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    }
}

#[derive(Deserialize)]
struct Upper {
    s: String,
}

struct Flaky {
    attempts: RefCell<usize>,
    fail_status: u16,
    fail_times: usize,
}

impl Provider for Flaky {
    fn complete(&self, _req: &Request) -> Result<Response, Error> {
        let mut n = self.attempts.borrow_mut();
        *n += 1;
        if *n <= self.fail_times {
            return Err(Error::Http {
                status: self.fail_status,
                message: format!("openai: {}: flaky", self.fail_status),
            });
        }
        Ok(Response {
            message: assistant("ok"),
            usage: Usage::default(),
            stop_reason: String::new(),
        })
    }

    fn stream(&self, req: &Request, _cancel: &Arc<AtomicBool>) -> StreamHandle {
        let (tx, rx) = mpsc::channel();
        let resp = match self.complete(req) {
            Ok(r) => r,
            Err(e) => {
                let thread = std::thread::spawn(move || Err(e));
                return StreamHandle::new(rx, thread);
            }
        };
        let thread = std::thread::spawn(move || {
            let _ = tx.send(StreamEvent::Content(resp.message.content.clone()));
            let _ = tx.send(StreamEvent::Done);
            Ok(resp)
        });
        StreamHandle::new(rx, thread)
    }
}

#[test]
fn run_retries_429_then_succeeds() {
    let p = Flaky {
        attempts: RefCell::new(0),
        fail_status: 429,
        fail_times: 2,
    };
    let mut a = Agent::new(p);
    let out = a.run(&[user("go")]).expect("run");
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].content, "ok");
    assert_eq!(*a.provider().attempts.borrow(), 3);
}

#[test]
fn run_does_not_retry_400() {
    let p = Flaky {
        attempts: RefCell::new(0),
        fail_status: 400,
        fail_times: 10,
    };
    let mut a = Agent::new(p);
    let err = a.run(&[user("go")]).unwrap_err();
    assert!(err.to_string().contains("flaky"), "got: {err}");
    assert_eq!(*a.provider().attempts.borrow(), 1);
}

#[test]
fn run_executes_tools_and_returns_transcript() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let ev = events.clone();
    let upper = new_tool("upper", "uppercase", "{}", |a: Upper| a.s.to_uppercase());
    let p = Fake {
        responses: RefCell::new(VecDeque::from([
            Response {
                message: call_tool("c1", "upper", r#"{"s":"hi"}"#),
                usage: Usage {
                    input: 10,
                    output: 5,
                    cached_input: 0,
                },
                stop_reason: String::new(),
            },
            Response {
                message: assistant("done"),
                usage: Usage {
                    input: 20,
                    output: 2,
                    cached_input: 0,
                },
                stop_reason: String::new(),
            },
        ])),
        ..Default::default()
    };
    let mut a = Agent::new(p)
        .tools(vec![upper])
        .max_turns(5)
        .on(move |e| ev.borrow_mut().push(e));
    let in_msgs = vec![user("go")];
    let out = a.run(&in_msgs).expect("run");

    assert_eq!(in_msgs.len(), 1);
    assert_eq!(in_msgs[0].content, "go");
    assert_eq!(out.len(), 4);
    assert_eq!(out[2].role, "tool");
    assert_eq!(out[2].tool_call_id, "c1");
    assert_eq!(out[2].content, "HI");
    assert_eq!(out[3].content, "done");

    let events = events.borrow();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].usage.input, 10);
    assert_eq!(events[0].usage.output, 5);
    assert_eq!(events[1].usage, Usage::default());
    assert_eq!(events[2].usage.input, 20);

    let reqs = a.provider().requests.borrow();
    let last = reqs.last().unwrap();
    assert_eq!(last.len(), 3);
    assert_eq!(last[2].content, "HI");
}

#[derive(Deserialize)]
struct Empty {}

#[test]
fn run_continues_on_tool_error() {
    let boom = new_tool("boom", "always fails", "{}", |_: Empty| {
        "error: boom".to_string()
    });
    let p = Fake {
        responses: RefCell::new(VecDeque::from([
            Response {
                message: call_tool("c1", "boom", "{}"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
            Response {
                message: assistant("recovered"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
        ])),
        ..Default::default()
    };
    let mut a = Agent::new(p).tools(vec![boom]);
    let out = a.run(&[user("go")]).expect("run");
    assert_eq!(out[2].content, "error: boom");
    assert_eq!(out[3].content, "recovered");
}

#[test]
fn run_unknown_tool() {
    let p = Fake {
        responses: RefCell::new(VecDeque::from([
            Response {
                message: call_tool("c1", "nope", "{}"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
            Response {
                message: assistant("ok"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
        ])),
        ..Default::default()
    };
    let mut a = Agent::new(p);
    let out = a.run(&[user("go")]).expect("run");
    assert!(out[2].content.contains("unknown tool"));
}

#[test]
fn run_max_turns() {
    let responses: VecDeque<Response> = (0..10)
        .map(|_| Response {
            message: call_tool("c", "x", "{}"),
            usage: Usage::default(),
            stop_reason: String::new(),
        })
        .collect();
    let p = Fake {
        responses: RefCell::new(responses),
        ..Default::default()
    };
    let mut a = Agent::new(p).max_turns(3);
    let err = a.run(&[user("go")]).unwrap_err();
    match err {
        Error::MaxTurns(h) => assert_eq!(h.len(), 1 + 3 * 2),
        e => panic!("want MaxTurns, got {e:?}"),
    }
}

#[derive(Deserialize)]
struct NeedsInt {
    n: i64,
}

#[test]
fn new_tool_bad_arguments() {
    let tool = new_tool("bad", "needs int", "{}", |args: NeedsInt| {
        args.n.to_string()
    });
    let got = (tool.run)(r#"{"n":"x"}"#, &mut |_| {});
    assert!(got.contains("invalid arguments"), "got: {got}");
    assert!(got.contains("Received:"), "got: {got}");
    assert_eq!((tool.run)(r#"{"n":42}"#, &mut |_| {}), "42");
}

#[derive(Deserialize)]
struct NeedsFields {
    n: i64,
    b: bool,
    s: String,
    #[serde(default)]
    o: Option<i64>,
}

#[test]
fn new_tool_coerces_argument_types() {
    let tool = new_tool(
        "coerce",
        "coerces types",
        r#"{"type":"object","properties":{"n":{"type":"integer"},"b":{"type":"boolean"},"s":{"type":"string"},"o":{"type":"integer"}},"required":["n","b","s"]}"#,
        |args: NeedsFields| format!("{} {} {} {:?}", args.n, args.b, args.s, args.o),
    );
    assert_eq!(
        (tool.run)(r#"{"n":"7","b":1,"s":42}"#, &mut |_| {}),
        "7 true 42 None"
    );
    assert_eq!(
        (tool.run)(r#"{"n":true,"b":"false","s":null}"#, &mut |_| {}),
        "1 false  None"
    );
    assert_eq!(
        (tool.run)(r#"{"n":null,"b":0,"s":"x","o":null}"#, &mut |_| {}),
        "0 false x None"
    );
}

#[test]
fn run_length_stop_does_not_execute_tool_calls() {
    let ran = Arc::new(AtomicBool::new(false));
    let ran2 = ran.clone();
    let boom = new_tool("boom", "records execution", "{}", move |_: Empty| {
        ran2.store(true, std::sync::atomic::Ordering::Relaxed);
        "executed".to_string()
    });
    let p = Fake {
        responses: RefCell::new(VecDeque::from([
            Response {
                message: call_tool("c1", "boom", r#"{"truncated":"args"}"#),
                usage: Usage::default(),
                stop_reason: "length".into(),
            },
            Response {
                message: assistant("recovered"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
        ])),
        ..Default::default()
    };
    let mut a = Agent::new(p).tools(vec![boom]);
    let out = a.run(&[user("go")]).expect("run");
    assert!(
        !ran.load(std::sync::atomic::Ordering::Relaxed),
        "tool must not execute on length stop"
    );
    assert!(
        out[2].content.contains("not executed"),
        "got: {}",
        out[2].content
    );
    assert!(
        out[2].content.contains("truncated"),
        "got: {}",
        out[2].content
    );
    assert_eq!(out[3].content, "recovered");
}

#[test]
fn run_parallel_tools_ordered() {
    let p = Fake {
        responses: RefCell::new(VecDeque::from([
            Response {
                message: Message {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "c1".into(),
                            name: "t1".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "c2".into(),
                            name: "t2".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "c3".into(),
                            name: "t3".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    tool_call_id: String::new(),
                },
                usage: Usage::default(),
                stop_reason: String::new(),
            },
            Response {
                message: assistant("done"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
        ])),
        ..Default::default()
    };
    let t1 = new_tool("t1", "", "{}", |_: Empty| "R1".into());
    let t2 = new_tool("t2", "", "{}", |_: Empty| "R2".into());
    let t3 = new_tool("t3", "", "{}", |_: Empty| "R3".into());
    let mut a = Agent::new(p).tools(vec![t1, t2, t3]);
    let out = a.run(&[user("go")]).expect("run");
    assert_eq!(out[2].tool_call_id, "c1");
    assert_eq!(out[2].content, "R1");
    assert_eq!(out[3].tool_call_id, "c2");
    assert_eq!(out[3].content, "R2");
    assert_eq!(out[4].tool_call_id, "c3");
    assert_eq!(out[4].content, "R3");
    assert_eq!(out[5].content, "done");
}

struct Steering {
    polled: usize,
}

impl axe::run::Sink for Steering {
    fn pending_user_input(&mut self) -> Option<String> {
        self.polled += 1;
        match self.polled {
            1 => Some("steer one".into()),
            2 => Some("steer two".into()),
            _ => None,
        }
    }
}

#[test]
fn run_injects_steering_between_turns() {
    let p = Fake {
        responses: RefCell::new(VecDeque::from([
            Response {
                message: assistant("first"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
            Response {
                message: assistant("second"),
                usage: Usage::default(),
                stop_reason: String::new(),
            },
        ])),
        ..Default::default()
    };
    let opts = axe::run::RunOptions {
        model: "m",
        system: "",
        tools: &[],
        max_turns: 5,
    };
    let mut sink = Steering { polled: 0 };
    let end = axe::run::run_stream(
        &p,
        &opts,
        &[user("go")],
        &Arc::new(AtomicBool::new(false)),
        &mut sink,
    );
    assert!(
        matches!(end.outcome, axe::run::Outcome::Done),
        "{:?}",
        end.outcome
    );
    assert_eq!(end.messages[1].role, "user");
    assert_eq!(end.messages[1].content, "steer one");
    assert_eq!(end.messages[2].content, "first");
    assert_eq!(end.messages[3].role, "user");
    assert_eq!(end.messages[3].content, "steer two");
    assert_eq!(end.messages[4].content, "second");
}

#[test]
fn compact_generates_structured_summary() {
    let p = Fake {
        responses: RefCell::new(VecDeque::from([Response {
            message: assistant(
                "## Goal\nbuild the thing\n## User Requirements\n- none\n## Progress\n### Done\n- none\n### In Progress\n- build\n### Blocked\n- none\n## Key Decisions\n- none\n## Files\n- none\n## Commands and Results\n- none\n## Open Questions\n- none\n## Next Steps\n1. build\n## Critical Context\n- none",
            ),
            usage: Usage::default(),
            stop_reason: String::new(),
        }])),
        ..Default::default()
    };
    let entries: Vec<axe::session::Entry> = (0..30)
        .map(|i| axe::session::Entry::Message {
            message: user(&format!("message {i} {}", "x".repeat(3800))),
        })
        .collect();
    let (summary, tokens_before, retained) =
        axe::session::compact(&p, "m1", &entries).expect("compact");
    assert!(summary.starts_with("## Goal\nbuild the thing"));
    assert_eq!(tokens_before, 0);
    assert!(!retained.is_empty() && retained.len() < entries.len());
    let reqs = p.requests.borrow();
    let last = reqs.last().unwrap();
    assert!(
        last[0].content.contains("## Goal"),
        "missing structured prompt"
    );
    assert!(last[0].content.contains("## Critical Context"));
}

struct MidStreamFlaky {
    attempts: RefCell<usize>,
}

impl Provider for MidStreamFlaky {
    fn complete(&self, _req: &Request) -> Result<Response, Error> {
        unreachable!("mid-stream flaky fake only streams")
    }

    fn stream(&self, _req: &Request, _cancel: &Arc<AtomicBool>) -> StreamHandle {
        let (tx, rx) = mpsc::channel();
        let n = {
            let mut a = self.attempts.borrow_mut();
            *a += 1;
            *a
        };
        let thread = std::thread::spawn(move || {
            let _ = tx.send(StreamEvent::Content("partial".into()));
            if n == 1 {
                return Err(Error::Http {
                    status: 429,
                    message: "flaky mid-stream".into(),
                });
            }
            let _ = tx.send(StreamEvent::Done);
            Ok(Response {
                message: assistant("done"),
                usage: Usage::default(),
                stop_reason: String::new(),
            })
        });
        StreamHandle::new(rx, thread)
    }
}

#[test]
fn run_does_not_retry_after_events_were_emitted() {
    let p = MidStreamFlaky {
        attempts: RefCell::new(0),
    };
    let mut a = Agent::new(p);
    let err = a.run(&[user("go")]).unwrap_err();
    assert!(err.to_string().contains("flaky mid-stream"), "got: {err}");
    assert_eq!(*a.provider().attempts.borrow(), 1);
}

#[test]
fn trim_trailing_tool_messages() {
    // A complete exchange is kept.
    let mut complete = vec![
        user("go"),
        call_tool("c1", "read", "{}"),
        Message {
            role: "tool".into(),
            content: "file contents".into(),
            tool_calls: Vec::new(),
            tool_call_id: "c1".into(),
        },
    ];
    axe::session::trim_trailing_tool_messages(&mut complete);
    assert_eq!(complete.len(), 3);

    // A partial exchange (2 calls, 1 result) is dropped whole, including
    // its dangling assistant message.
    let mut partial = vec![
        user("go"),
        Message {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "c2".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            ],
            tool_call_id: String::new(),
        },
        Message {
            role: "tool".into(),
            content: "file contents".into(),
            tool_calls: Vec::new(),
            tool_call_id: "c1".into(),
        },
    ];
    axe::session::trim_trailing_tool_messages(&mut partial);
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].role, "user");

    let mut msgs2 = vec![user("go")];
    axe::session::trim_trailing_tool_messages(&mut msgs2);
    assert_eq!(msgs2.len(), 1);
}

struct NopSink;

impl axe::run::Sink for NopSink {}

/// Cancelling mid-batch must still produce a provider-valid transcript:
/// every tool call gets a result, even the ones never executed.
#[test]
fn run_cancel_mid_batch_synthesizes_results() {
    struct CancelProvider;

    impl Provider for CancelProvider {
        fn complete(&self, _req: &Request) -> Result<Response, Error> {
            unreachable!("cancel fake only streams")
        }

        fn stream(&self, _req: &Request, _cancel: &Arc<AtomicBool>) -> StreamHandle {
            let (tx, rx) = mpsc::channel();
            let calls = vec![
                ToolCall {
                    id: "c1".into(),
                    name: "boom".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "c2".into(),
                    name: "boom".into(),
                    arguments: "{}".into(),
                },
            ];
            let thread = std::thread::spawn(move || {
                for c in &calls {
                    let _ = tx.send(StreamEvent::ToolCall(c.clone()));
                }
                let _ = tx.send(StreamEvent::Done);
                Ok(Response {
                    message: Message {
                        role: "assistant".into(),
                        content: String::new(),
                        tool_calls: calls,
                        tool_call_id: String::new(),
                    },
                    usage: Usage::default(),
                    stop_reason: String::new(),
                })
            });
            StreamHandle::new(rx, thread)
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    let mut boom = new_tool("boom", "", "{}", move |_: Empty| {
        c2.store(true, std::sync::atomic::Ordering::Relaxed);
        "did work".to_string()
    });
    boom.sequential = true;
    let opts = axe::run::RunOptions {
        model: "m",
        system: "",
        tools: &[boom],
        max_turns: 5,
    };
    let end = axe::run::run_stream(&CancelProvider, &opts, &[user("go")], &cancel, &mut NopSink);
    assert!(
        matches!(end.outcome, axe::run::Outcome::Cancelled),
        "{:?}",
        end.outcome
    );
    let msgs = end.messages;
    let pos = msgs
        .iter()
        .rposition(|m| !m.tool_calls.is_empty())
        .expect("assistant with tool calls");
    let results = &msgs[pos + 1..];
    assert_eq!(results.len(), 2, "every call needs a result");
    assert_eq!(results[0].content, "did work");
    assert!(
        results[1].content.contains("interrupted"),
        "got: {}",
        results[1].content
    );

    // The transcript survives the pre-turn validity trim untouched.
    let mut trimmed = msgs.clone();
    axe::session::trim_trailing_tool_messages(&mut trimmed);
    assert_eq!(trimmed.len(), msgs.len());
}

#[test]
fn run_compacts_after_complete_tool_batch() {
    struct CompactSink;

    impl axe::run::Sink for CompactSink {
        fn should_compact(&mut self, input: usize, output: usize) -> bool {
            input + output > 250_000
        }
    }

    let provider = Fake {
        responses: RefCell::new(VecDeque::from([Response {
            message: call_tool("call-1", "upper", r#"{"s":"done"}"#),
            usage: Usage {
                input: 249_900,
                output: 200,
                cached_input: 0,
            },
            stop_reason: String::new(),
        }])),
        ..Default::default()
    };
    let tool = new_tool("upper", "", r#"{"type":"object"}"#, |args: Upper| {
        args.s.to_uppercase()
    });
    let opts = axe::run::RunOptions {
        model: "m",
        system: "",
        tools: &[tool],
        max_turns: 5,
    };
    let mut sink = CompactSink;
    let end = axe::run::run_stream(
        &provider,
        &opts,
        &[user("work")],
        &Arc::new(AtomicBool::new(false)),
        &mut sink,
    );
    assert!(matches!(end.outcome, axe::run::Outcome::Compact));
    assert_eq!(end.messages.len(), 3);
    assert_eq!(end.messages[1].tool_calls[0].id, "call-1");
    assert_eq!(end.messages[2].role, "tool");
    assert_eq!(end.messages[2].tool_call_id, "call-1");
    assert_eq!(end.messages[2].content, "DONE");
}
