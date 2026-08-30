//! OpenAI-compatible chat completions provider.

use crate::{Error, Message, Provider, Request, Response, StreamHandle, ToolCall, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

pub struct OpenAI {
    base_url: String,
    api_key: String,
}

type BuiltRequest = (String, Vec<(String, String)>, Vec<u8>);

pub use crate::StreamEvent;

impl OpenAI {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        OpenAI {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    pub fn list_models(&self) -> Result<Vec<String>, Error> {
        let url = format!("{}/models", self.base_url);
        let headers = vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.api_key),
        )];
        let resp = crate::http::get(&url, &headers).map_err(Error::Transport)?;
        if resp.status != 200 {
            return Err(Error::Provider(format!(
                "openai: models: unexpected status {}",
                resp.status
            )));
        }
        let v: Value = serde_json::from_slice(&resp.body).map_err(err)?;
        let mut out = Vec::new();
        if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
            for item in data {
                if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    out.push(id.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn complete_stream(
        &self,
        req: &Request,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        tx: std::sync::mpsc::Sender<StreamEvent>,
    ) -> std::thread::JoinHandle<Result<Response, Error>> {
        match self.build_request(req, true) {
            Ok((url, headers, body)) => {
                let c2 = cancel.clone();
                let tx2 = tx.clone();
                std::thread::spawn(move || {
                    run_request(&url, &headers, &body, true, &c2, Some(&tx2))
                })
            }
            Err(e) => std::thread::spawn(move || Err(e)),
        }
    }

    fn build_request(&self, req: &Request, stream: bool) -> Result<BuiltRequest, Error> {
        let mut msgs = Vec::with_capacity(req.messages.len() + 1);
        if !req.system.is_empty() {
            msgs.push(OaMessage {
                role: "system".into(),
                content: Some(req.system.to_string()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        for m in req.messages {
            let tool_calls = if m.tool_calls.is_empty() {
                None
            } else {
                Some(
                    m.tool_calls
                        .iter()
                        .map(|c| OaToolCall {
                            id: c.id.clone(),
                            r#type: "function".into(),
                            function: OaFunction {
                                name: c.name.clone(),
                                arguments: c.arguments.clone(),
                            },
                        })
                        .collect(),
                )
            };
            msgs.push(OaMessage {
                role: m.role.clone(),
                content: if m.content.is_empty() && m.role != "tool" {
                    None
                } else {
                    Some(m.content.clone())
                },
                tool_calls,
                tool_call_id: if m.tool_call_id.is_empty() {
                    None
                } else {
                    Some(m.tool_call_id.clone())
                },
            });
        }

        let mut tools = Vec::new();
        for t in req.tools {
            tools.push(OaTool {
                r#type: "function".into(),
                function: OaToolFunction {
                    name: t.name.to_string(),
                    description: t.description.to_string(),
                    parameters: t.parameters.clone(),
                },
            });
        }

        let body = serde_json::to_vec(&OaRequest {
            model: req.model,
            messages: msgs,
            tools,
            stream,
        })
        .map_err(err)?;
        let url = format!("{}/chat/completions", self.base_url);
        let mut headers = Vec::new();
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        headers.push((
            "Authorization".to_string(),
            format!("Bearer {}", self.api_key),
        ));
        Ok((url, headers, body))
    }
}

fn run_request(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    stream: bool,
    cancel: &Arc<AtomicBool>,
    tx: Option<&Sender<StreamEvent>>,
) -> Result<Response, Error> {
    let mut easy = crate::curlffi::Easy::new().map_err(Error::Transport)?;
    easy.url(url).map_err(err)?;
    easy.post().map_err(err)?;
    // SAFETY: curl may or may not copy POSTFIELDS; `body` outlives perform
    // in every caller either way.
    easy.post_fields(body).map_err(err)?;
    easy.fail_on_error(false).map_err(err)?;
    easy.connect_timeout(10).map_err(err)?;
    // Abort if the connection stalls (<1 byte/s for 60s): a slow-drip
    // server must not hang the run forever.
    easy.low_speed(1, 60).map_err(err)?;
    easy.headers(headers).map_err(err)?;

    let (status, acc) = {
        let acc = Rc::new(RefCell::new(StreamAcc::default()));
        let mut state = ReqState {
            acc: acc.clone(),
            cancel: cancel.clone(),
            tx: tx.cloned(),
        };
        let mut transfer = easy.transfer();
        transfer.write_function(write_cb, &mut state as *mut ReqState as *mut c_void);
        if stream {
            transfer.progress_function(progress_cb, &mut state as *mut ReqState as *mut c_void);
        }
        transfer.perform().map_err(|e| {
            if cancel.load(Ordering::Relaxed) {
                Error::Provider("interrupted".into())
            } else {
                Error::Transport(e)
            }
        })?;
        let status = easy.response_code().map_err(err)? as u16;
        let acc = match Rc::try_unwrap(acc) {
            Ok(cell) => cell.into_inner(),
            Err(rc) => rc.borrow().clone(),
        };
        (status, acc)
    };

    if stream && status == 200 {
        // A 200 with a body that never produced a recognized SSE event
        // (plain JSON error, proxy page) must not become a silent empty
        // assistant turn.
        if acc.events == 0 {
            let snippet = String::from_utf8_lossy(&acc.raw[..acc.raw.len().min(200)]).into_owned();
            return Err(Error::Provider(format!(
                "openai: invalid response body: {snippet}"
            )));
        }
        let response = acc.response();
        acc.finish(tx);
        return Ok(response);
    }
    let resp = crate::http::Response {
        status,
        body: acc.raw,
    };

    if resp.status != 200 {
        let mut msg = String::new();
        if let Ok(e) = serde_json::from_slice::<OaError>(&resp.body) {
            msg = e.error.message;
        }
        if !msg.is_empty() {
            return Err(Error::Http {
                status: resp.status,
                message: format!("openai: {}: {}", resp.status, msg),
            });
        }
        return Err(Error::Http {
            status: resp.status,
            message: format!("openai: unexpected status {}", resp.status),
        });
    }

    let parsed: OaResponse = serde_json::from_slice(&resp.body).map_err(err)?;
    if parsed.choices.is_empty() {
        return Err(Error::Provider("openai: no choices in response".into()));
    }
    let choice = &parsed.choices[0];
    let stop_reason = choice.finish_reason.clone().unwrap_or_default();
    let om = &choice.message;
    let mut calls = Vec::new();
    if let Some(cs) = &om.tool_calls {
        for c in cs {
            calls.push(ToolCall {
                id: c.id.clone(),
                name: c.function.name.clone(),
                arguments: c.function.arguments.clone(),
            });
        }
    }
    Ok(Response {
        message: Message {
            role: "assistant".into(),
            content: om.content.clone().unwrap_or_default(),
            tool_calls: calls,
            tool_call_id: String::new(),
        },
        usage: Usage {
            input: parsed.usage.prompt_tokens,
            output: parsed.usage.completion_tokens,
            cached_input: parsed.usage.prompt_tokens_details.cached_tokens,
        },
        stop_reason,
    })
}
impl Provider for OpenAI {
    fn complete(&self, req: &Request) -> Result<Response, Error> {
        let (url, headers, body) = self.build_request(req, false)?;
        run_request(
            &url,
            &headers,
            &body,
            false,
            &Arc::new(AtomicBool::new(false)),
            None,
        )
    }

    fn stream(&self, req: &Request, cancel: &Arc<AtomicBool>) -> StreamHandle {
        let (tx, rx) = std::sync::mpsc::channel();
        StreamHandle::new(rx, self.complete_stream(req, cancel, tx))
    }
}

#[derive(Clone, Default)]
struct StreamAcc {
    /// Raw bytes as received; the body for non-stream mode.
    raw: Vec<u8>,
    /// SSE event buffer (drained as events complete).
    buf: Vec<u8>,
    content: String,
    calls: Vec<OaToolCallDelta>,
    usage: OaUsage,
    out_tokens: usize,
    finish_reason: Option<String>,
    /// Recognized SSE events seen ([DONE] or a parsed chunk).
    events: usize,
}

#[derive(Clone, Default, Deserialize)]
struct OaToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OaFunctionDelta>,
}

#[derive(Clone, Default, Deserialize)]
struct OaFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
struct OaDeltaMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaToolCallDelta>>,
}

#[derive(Clone, Default, Deserialize)]
struct OaStreamChunk {
    #[serde(default)]
    choices: Vec<OaStreamChoice>,
    #[serde(default)]
    usage: OaUsage,
}

#[derive(Clone, Default, Deserialize)]
struct OaStreamChoice {
    #[serde(default)]
    delta: OaDeltaMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

const MAX_STREAM_TOOL_CALLS: usize = 64;

impl StreamAcc {
    fn feed(&mut self, data: &[u8], tx: Option<&std::sync::mpsc::Sender<StreamEvent>>) {
        self.raw.extend_from_slice(data);
        if data.contains(&b'\r') || self.buf.last() == Some(&b'\r') {
            // Normalize CRLF to LF so event splitting works even when a
            // chunk boundary lands between "\r" and "\n".
            let mut prev_cr = self.buf.last() == Some(&b'\r');
            for &b in data {
                if b == b'\n' && prev_cr {
                    self.buf.pop();
                }
                self.buf.push(b);
                prev_cr = b == b'\r';
            }
        } else {
            self.buf.extend_from_slice(data);
        }
        loop {
            let sep = find_bytes(&self.buf, b"\n\n");
            let Some(sep) = sep else { break };
            let event = self.buf.drain(..sep + 2).collect::<Vec<u8>>();
            self.handle_event(&event, tx);
        }
    }

    fn handle_event(&mut self, event: &[u8], tx: Option<&std::sync::mpsc::Sender<StreamEvent>>) {
        let mut payload = String::new();
        for line in event.split(|&b| b == b'\n') {
            let line = std::str::from_utf8(line).unwrap_or("");
            if let Some(data) = line.strip_prefix("data:") {
                // SSE joins multiple data: lines with a newline.
                if !payload.is_empty() {
                    payload.push('\n');
                }
                payload.push_str(data.trim());
            }
        }
        if payload.is_empty() {
            return;
        }
        self.events += 1;
        if payload == "[DONE]" {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<OaStreamChunk>(&payload) else {
            return;
        };
        if let Some(tx) = tx
            && (chunk.usage.prompt_tokens != 0 || chunk.usage.completion_tokens != 0)
        {
            self.usage = chunk.usage;
            let _ = tx.send(StreamEvent::Tokens {
                input: self.usage.prompt_tokens,
                output: self.usage.completion_tokens,
                cached_input: self.usage.prompt_tokens_details.cached_tokens,
            });
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            return;
        };
        if let Some(fr) = &choice.finish_reason {
            self.finish_reason = Some(fr.clone());
        }
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            self.content.push_str(&content);
            self.out_tokens += 1;
            if let Some(tx) = tx {
                let _ = tx.send(StreamEvent::Content(content));
                let _ = tx.send(StreamEvent::Tokens {
                    input: self.usage.prompt_tokens,
                    output: self.out_tokens,
                    cached_input: self.usage.prompt_tokens_details.cached_tokens,
                });
            }
        }
        if let Some(calls) = choice.delta.tool_calls {
            for call in calls {
                if call.index >= MAX_STREAM_TOOL_CALLS {
                    continue;
                }
                let index = call.index;
                while self.calls.len() <= index {
                    self.calls.push(OaToolCallDelta::default());
                }
                let entry = &mut self.calls[index];
                if let Some(id) = call.id {
                    entry.id = Some(id);
                }
                if let Some(f) = call.function {
                    let func = entry.function.get_or_insert_with(Default::default);
                    if let Some(name) = f.name {
                        func.name = Some(name);
                    }
                    if let Some(args) = f.arguments {
                        func.arguments
                            .get_or_insert_with(String::new)
                            .push_str(&args);
                    }
                }
            }
        }
    }

    fn tool_calls(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .map(|call| ToolCall {
                id: call.id.clone().unwrap_or_default(),
                name: call
                    .function
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .unwrap_or_default(),
                arguments: call
                    .function
                    .as_ref()
                    .and_then(|f| f.arguments.clone())
                    .unwrap_or_default(),
            })
            .collect()
    }

    fn finish(&self, tx: Option<&std::sync::mpsc::Sender<StreamEvent>>) {
        if let Some(tx) = tx {
            for call in self.tool_calls() {
                let _ = tx.send(StreamEvent::ToolCall(call));
            }
            let _ = tx.send(StreamEvent::Done);
        }
    }

    fn response(&self) -> Response {
        Response {
            message: Message {
                role: "assistant".into(),
                content: self.content.clone(),
                tool_calls: self.tool_calls(),
                tool_call_id: String::new(),
            },
            usage: Usage {
                input: self.usage.prompt_tokens,
                output: self.usage.completion_tokens,
                cached_input: self.usage.prompt_tokens_details.cached_tokens,
            },
            stop_reason: self.finish_reason.clone().unwrap_or_default(),
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

struct ReqState {
    acc: Rc<RefCell<StreamAcc>>,
    cancel: Arc<AtomicBool>,
    tx: Option<Sender<StreamEvent>>,
}

unsafe extern "C" fn write_cb(
    ptr: *mut c_char,
    size: usize,
    nmemb: usize,
    userdata: *mut c_void,
) -> usize {
    let st = unsafe { &mut *(userdata as *mut ReqState) };
    let data = unsafe { std::slice::from_raw_parts(ptr as *const u8, size * nmemb) };
    st.acc.borrow_mut().feed(data, st.tx.as_ref());
    size * nmemb
}

unsafe extern "C" fn progress_cb(
    userdata: *mut c_void,
    _dltotal: f64,
    _dlnow: f64,
    _ultotal: f64,
    _ulnow: f64,
) -> c_int {
    let st = unsafe { &mut *(userdata as *mut ReqState) };
    if st.cancel.load(Ordering::Relaxed) {
        1
    } else {
        0
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::Provider(e.to_string())
}

#[derive(Serialize)]
struct OaRequest<'a> {
    model: &'a str,
    messages: Vec<OaMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OaTool>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Serialize, Deserialize)]
struct OaMessage {
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct OaToolCall {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    function: OaFunction,
}

#[derive(Serialize, Deserialize)]
struct OaFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct OaTool {
    #[serde(rename = "type")]
    r#type: String,
    function: OaToolFunction,
}

#[derive(Serialize)]
struct OaToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Deserialize)]
struct OaResponse {
    #[serde(default)]
    choices: Vec<OaChoice>,
    #[serde(default)]
    usage: OaUsage,
}

#[derive(Deserialize)]
struct OaChoice {
    message: OaMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OaErrorPayload {
    message: String,
}

#[derive(Clone, Deserialize, Default)]
struct OaUsage {
    #[serde(default, rename = "prompt_tokens")]
    prompt_tokens: usize,
    #[serde(default, rename = "completion_tokens")]
    completion_tokens: usize,
    #[serde(default)]
    prompt_tokens_details: OaPromptTokensDetails,
}

#[derive(Clone, Deserialize, Default)]
struct OaPromptTokensDetails {
    #[serde(default)]
    cached_tokens: usize,
}

#[derive(Deserialize)]
struct OaError {
    error: OaErrorPayload,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(acc: &mut StreamAcc, chunks: &[&[u8]]) -> Vec<StreamEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        for c in chunks {
            acc.feed(c, Some(&tx));
        }
        drop(tx);
        rx.try_iter().collect()
    }

    #[test]
    fn malformed_body_produces_no_events() {
        let mut acc = StreamAcc::default();
        feed_all(&mut acc, &[b"<html>502 Bad Gateway</html>"]);
        assert_eq!(acc.events, 0);
        assert_eq!(acc.raw, b"<html>502 Bad Gateway</html>");
    }

    #[test]
    fn sse_parses_crlf_events() {
        let mut acc = StreamAcc::default();
        let full = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\n";
        let got = feed_all(&mut acc, &[full]);
        assert!(
            got.iter()
                .any(|e| matches!(e, StreamEvent::Content(c) if c == "hi")),
            "{got:?}"
        );
    }

    #[test]
    fn sse_crlf_split_across_chunk_boundary() {
        let mut acc = StreamAcc::default();
        let full = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\n";
        let split = full.iter().position(|&b| b == b'\r').unwrap();
        let got = feed_all(&mut acc, &[&full[..split], &full[split..]]);
        assert!(
            got.iter()
                .any(|e| matches!(e, StreamEvent::Content(c) if c == "hi")),
            "{got:?}"
        );
    }

    #[test]
    fn sse_joins_multi_line_data_fields() {
        let mut acc = StreamAcc::default();
        let got = feed_all(
            &mut acc,
            &[b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}\ndata: ]}\n\n"],
        );
        assert!(
            got.iter()
                .any(|e| matches!(e, StreamEvent::Content(c) if c == "x")),
            "{got:?}"
        );
    }
}
