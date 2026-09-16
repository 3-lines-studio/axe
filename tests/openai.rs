//! Wire format round trip against a local HTTP server (plain http, no TLS
//! needed for the test).

use axe::openai::StreamEvent;
use axe::{Image, Message, OpenAI, Provider, Request, ToolCall, new_tool};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;

#[test]
fn openai_round_trip() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut sock, _) = server.accept().unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        let mut header_end = None;
        while header_end.is_none() {
            let n = sock.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            header_end = req.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
        }
        let header_end = header_end.unwrap();
        let head = String::from_utf8_lossy(&req[..header_end]);
        assert!(
            head.starts_with("POST /chat/completions HTTP/1.1"),
            "request line: {head}"
        );
        assert!(head.contains("Authorization: Bearer k1"), "headers: {head}");
        let cl: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.eq_ignore_ascii_case("content-length") {
                    v.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap();
        while req.len() < header_end + cl {
            let n = sock.read(&mut buf).unwrap();
            req.extend_from_slice(&buf[..n]);
        }
        let body: serde_json::Value =
            serde_json::from_slice(&req[header_end..header_end + cl]).unwrap();
        assert_eq!(body["model"], "m1");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be brief");
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "go");
        assert_eq!(blocks[1]["type"], "image_url");
        assert_eq!(blocks[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(msgs[2]["reasoning_content"], "because");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "c9");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a.txt"}"#
        );
        assert_eq!(msgs[3]["role"], "tool");
        let tool_blocks = msgs[3]["content"].as_array().unwrap();
        assert_eq!(tool_blocks[0]["type"], "text");
        assert_eq!(tool_blocks[0]["text"], "hi");
        assert_eq!(tool_blocks[1]["type"], "image_url");
        assert_eq!(
            tool_blocks[1]["image_url"]["url"],
            "data:image/png;base64,BBBB"
        );
        assert_eq!(msgs[3]["tool_call_id"], "c9");
        // A tool result without images stays a plain string, including empty.
        assert_eq!(msgs[4]["role"], "tool");
        assert_eq!(msgs[4]["content"], "");
        assert_eq!(msgs[4]["tool_call_id"], "c11");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "read");
        assert_eq!(
            tools[0]["function"]["parameters"],
            serde_json::json!({"x": 1})
        );
        let resp = br#"{"choices":[{"message":{"role":"assistant","reasoning_content":"thought","tool_calls":[{"id":"c10","type":"function","function":{"name":"read","arguments":"{\"path\":\"b.txt\"}"}}]}}],"usage":{"prompt_tokens":123,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":100}}}"#;
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: ");
        let _ = sock.write_all(resp.len().to_string().as_bytes());
        let _ = sock.write_all(b"\r\nConnection: close\r\n\r\n");
        let _ = sock.write_all(resp);
    });

    let p = OpenAI::new(format!("http://{addr}"), "k1");
    let req = Request {
        model: "m1",
        system: "be brief",
        messages: &[
            Message {
                role: "user".into(),
                content: "go".into(),
                tool_calls: Vec::new(),
                tool_call_id: String::new(),
                reasoning: String::new(),
                images: vec![Image {
                    path: "p.png".into(),
                    url: "data:image/png;base64,AAAA".into(),
                }],
            },
            Message {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c9".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                }],
                tool_call_id: String::new(),
                reasoning: "because".into(),
                images: Vec::new(),
            },
            Message {
                role: "tool".into(),
                content: "hi".into(),
                tool_calls: Vec::new(),
                tool_call_id: "c9".into(),
                reasoning: String::new(),
                images: vec![Image {
                    path: "shot.png".into(),
                    url: "data:image/png;base64,BBBB".into(),
                }],
            },
            Message {
                role: "tool".into(),
                content: String::new(),
                tool_calls: Vec::new(),
                tool_call_id: "c11".into(),
                reasoning: String::new(),
                images: Vec::new(),
            },
        ],
        tools: &[new_tool(
            "read",
            "d",
            r#"{"x":1}"#,
            |_: serde_json::Value| String::new(),
        )],
    };
    let resp = p.complete(&req).unwrap();
    handle.join().unwrap();

    assert_eq!(resp.message.role, "assistant");
    assert_eq!(resp.message.reasoning, "thought");
    assert_eq!(resp.message.tool_calls.len(), 1);
    let c = &resp.message.tool_calls[0];
    assert_eq!(c.id, "c10");
    assert_eq!(c.name, "read");
    assert_eq!(c.arguments, r#"{"path":"b.txt"}"#);
    assert_eq!(resp.usage.input, 123);
    assert_eq!(resp.usage.output, 7);
    assert_eq!(resp.usage.cached_input, 100);
}

#[test]
fn openai_stream_round_trip() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut sock, _) = server.accept().unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).unwrap();
            req.extend_from_slice(&buf[..n]);
            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hard\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n",
            "data: [DONE]\n\n"
        );
        write!(
            sock,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let p = OpenAI::new(format!("http://{addr}"), "k1");
    let req = Request {
        model: "m1",
        system: "",
        messages: &[Message {
            role: "user".into(),
            content: "go".into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
            reasoning: String::new(),
            images: Vec::new(),
        }],
        tools: &[],
    };
    let (tx, rx) = mpsc::channel();
    let response = p
        .complete_stream(&req, &Arc::new(AtomicBool::new(false)), tx)
        .join()
        .unwrap()
        .unwrap();
    handle.join().unwrap();

    assert_eq!(response.message.content, "hi");
    assert_eq!(response.message.reasoning, "think hard");
    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.message.tool_calls[0].arguments, r#"{"path":"a"}"#);
    assert_eq!(response.usage.input, 4);
    assert_eq!(response.usage.output, 2);
    assert_eq!(response.usage.cached_input, 3);
    assert_eq!(response.stop_reason, "length");
    let events: Vec<_> = rx.into_iter().collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done))
    );
    assert!(events.iter().any(|event| {
        matches!(event, StreamEvent::ToolCall(call) if call.arguments == r#"{"path":"a"}"#)
    }));
}

#[test]
fn openai_error_status() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut sock, _) = server.accept().unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let resp = br#"{"error":{"message":"Invalid API key"}}"#;
        let _ = sock.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: ");
        let _ = sock.write_all(resp.len().to_string().as_bytes());
        let _ = sock.write_all(b"\r\nConnection: close\r\n\r\n");
        let _ = sock.write_all(resp);
    });

    let p = OpenAI::new(format!("http://{addr}"), "bad");
    let req = Request {
        model: "m1",
        system: "",
        messages: &[Message {
            role: "user".into(),
            content: "go".into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
            reasoning: String::new(),
            images: Vec::new(),
        }],
        tools: &[],
    };
    let err = p.complete(&req).unwrap_err();
    handle.join().unwrap();
    let msg = err.to_string();
    assert!(msg.contains("openai: 401"), "got: {msg}");
    assert!(msg.contains("Invalid API key"), "got: {msg}");
}

#[test]
fn openai_empty_assistant_keeps_content() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut sock, _) = server.accept().unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).unwrap();
            req.extend_from_slice(&buf[..n]);
            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = req
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap()
            + 4;
        let cl: usize = String::from_utf8_lossy(&req[..header_end])
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.eq_ignore_ascii_case("content-length") {
                    v.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap();
        while req.len() < header_end + cl {
            let n = sock.read(&mut buf).unwrap();
            req.extend_from_slice(&buf[..n]);
        }
        let body: serde_json::Value =
            serde_json::from_slice(&req[header_end..header_end + cl]).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "");
        assert_eq!(msgs[1].get("tool_calls"), None);
        let resp = br#"{"choices":[{"message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":0}}}"#;
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: ");
        let _ = sock.write_all(resp.len().to_string().as_bytes());
        let _ = sock.write_all(b"\r\nConnection: close\r\n\r\n");
        let _ = sock.write_all(resp);
    });

    let p = OpenAI::new(format!("http://{addr}"), "k1");
    let req = Request {
        model: "m1",
        system: "",
        messages: &[
            Message {
                role: "user".into(),
                content: "hi".into(),
                tool_calls: Vec::new(),
                tool_call_id: String::new(),
                reasoning: String::new(),
                images: Vec::new(),
            },
            Message {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: Vec::new(),
                tool_call_id: String::new(),
                reasoning: String::new(),
                images: Vec::new(),
            },
        ],
        tools: &[],
    };
    p.complete(&req).unwrap();
    handle.join().unwrap();
}
