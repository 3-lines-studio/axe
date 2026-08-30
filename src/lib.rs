//! Minimal LLM coding agent harness.
//!
//! The loop is the only logic: messages -> LLM -> tool calls -> results ->
//! repeat. It lives in `run`, is shared by the SDK and the TUI, and never
//! mutates its input.
//!
//! Message/ToolCall serialize with PascalCase field names (session
//! storage); the OpenAI provider maps them to the wire format.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

pub mod curlffi;
mod http;
pub mod markdown;
pub mod openai;
pub mod run;
pub mod session;
pub mod term;
pub mod tools;
pub mod tui;

pub use openai::OpenAI;

/// Write via temp file + rename in the destination directory so a crash or
/// full disk never leaves a truncated file behind.
pub fn atomic_write(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TAG: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let parent = dir.unwrap_or(std::path::Path::new("."));
    let tag = TAG.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".axe-tmp-{}-{tag}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0)
    ));
    let permissions = std::fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| metadata.permissions());
    let res = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(data)?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            Ok(())
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Arguments")]
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "Role")]
    pub role: String,
    #[serde(rename = "Content")]
    pub content: String,
    #[serde(rename = "ToolCalls", default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(
        rename = "ToolCallID",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub tool_call_id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: usize,
    pub output: usize,
    pub cached_input: usize,
}

#[derive(Debug)]
pub enum StreamEvent {
    Content(String),
    ToolCall(ToolCall),
    Tokens {
        input: usize,
        output: usize,
        cached_input: usize,
    },
    Done,
}

pub struct StreamHandle {
    rx: mpsc::Receiver<StreamEvent>,
    thread: std::thread::JoinHandle<Result<Response, Error>>,
}

impl StreamHandle {
    pub fn new(
        rx: mpsc::Receiver<StreamEvent>,
        thread: std::thread::JoinHandle<Result<Response, Error>>,
    ) -> Self {
        StreamHandle { rx, thread }
    }

    pub fn events(&self) -> &mpsc::Receiver<StreamEvent> {
        &self.rx
    }

    pub fn join(self) -> Result<Response, Error> {
        self.thread
            .join()
            .map_err(|_| Error::Provider("request thread panicked".into()))?
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub turn: usize,
    pub message: Message,
    pub usage: Usage,
}

#[derive(Debug)]
pub enum Error {
    MaxTurns(Vec<Message>),
    /// Connection-level failure (DNS, TLS, refused, timeout): retryable.
    Transport(String),
    Provider(String),
    Http {
        status: u16,
        message: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::MaxTurns(_) => write!(f, "axe: max turns reached"),
            Error::Transport(s) => write!(f, "{s}"),
            Error::Provider(s) => write!(f, "{s}"),
            Error::Http { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// One-line hint for the system prompt's tool list.
    pub snippet: &'static str,
    /// When true, tool calls in a batch run one after another instead of in
    /// parallel (used by file-mutating tools to avoid races).
    pub sequential: bool,
    #[allow(clippy::type_complexity)]
    pub run: Box<dyn Fn(&str, &mut dyn FnMut(&str)) -> String + Send + Sync>,
}

pub fn new_tool<T>(
    name: &'static str,
    description: &'static str,
    schema: &'static str,
    run: impl Fn(T) -> String + Send + Sync + 'static,
) -> Tool
where
    T: DeserializeOwned,
{
    new_tool_with_progress(name, description, schema, move |args, _progress| run(args))
}

/// Like `new_tool`, but the run closure also receives a progress callback it
/// can call with partial output while working (e.g. live bash output).
pub fn new_tool_with_progress<T>(
    name: &'static str,
    description: &'static str,
    schema: &'static str,
    run: impl Fn(T, &mut dyn FnMut(&str)) -> String + Send + Sync + 'static,
) -> Tool
where
    T: DeserializeOwned,
{
    let parameters: Value = serde_json::from_str(schema).unwrap_or(Value::Null);
    let schema = parameters.clone();
    Tool {
        name,
        description,
        parameters,
        snippet: "",
        sequential: false,
        run: Box::new(move |raw, progress| {
            let raw = if raw.trim().is_empty() { "{}" } else { raw };
            let mut args: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            if !args.is_null() {
                coerce_args(&mut args, &schema);
            }
            let coerced = serde_json::to_string(&args).unwrap_or_else(|_| raw.to_string());
            match serde_json::from_str::<T>(&coerced) {
                Ok(args) => run(args, progress),
                Err(e) => format!("error: invalid arguments for {name}: {e}\nReceived: {raw}"),
            }
        }),
    }
}

/// Coerce LLM arguments toward the declared JSON schema before deserializing.
/// Models frequently send numbers as strings, booleans as 1/0, or null for
/// optional fields; serde would reject those outright.
fn coerce_args(args: &mut Value, schema: &Value) {
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object())
        && let Value::Object(map) = args
    {
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|r| r.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        for (key, prop) in props {
            if let Some(v) = map.get_mut(key) {
                if v.is_null() && !required.contains(&key.as_str()) && !accepts_null(prop) {
                    map.remove(key);
                    continue;
                }
                coerce_value(v, prop);
                coerce_args(v, prop);
            }
        }
    }
    if let Some(items) = schema.get("items")
        && let Value::Array(arr) = args
    {
        for item in arr.iter_mut() {
            coerce_value(item, items);
            coerce_args(item, items);
        }
    }
}

fn accepts_null(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(ts)) => ts.iter().any(|t| t.as_str() == Some("null")),
        _ => false,
    }
}

fn coerce_value(value: &mut Value, schema: &Value) {
    let ty = schema.get("type").and_then(|t| t.as_str());
    match ty {
        Some("number") | Some("integer") => {
            if let Value::String(s) = value {
                if let Ok(n) = s.trim().parse::<f64>()
                    && n.is_finite()
                {
                    if ty == Some("integer") && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
                        // Fractional values are truncated: models send "3.5"
                        // for an integer field more often than they mean it.
                        *value = Value::Number(serde_json::Number::from(n as i64));
                    } else if ty == Some("number")
                        && let Some(num) = serde_json::Number::from_f64(n)
                    {
                        *value = Value::Number(num);
                    }
                }
            } else if value.is_null() {
                *value = Value::Number(serde_json::Number::from(0));
            } else if let Some(b) = value.as_bool() {
                *value = Value::Number(serde_json::Number::from(if b { 1 } else { 0 }));
            }
        }
        Some("boolean") => {
            if let Value::String(s) = value {
                match s.trim() {
                    "true" => *value = Value::Bool(true),
                    "false" => *value = Value::Bool(false),
                    _ => {}
                }
            } else if let Some(n) = value.as_f64() {
                if n == 1.0 {
                    *value = Value::Bool(true);
                } else if n == 0.0 {
                    *value = Value::Bool(false);
                }
            } else if value.is_null() {
                *value = Value::Bool(false);
            }
        }
        Some("string") => {
            if value.is_number() || value.is_boolean() {
                *value = Value::String(value.to_string());
            } else if value.is_null() {
                *value = Value::String(String::new());
            }
        }
        _ => {}
    }
}

pub trait Provider {
    fn complete(&self, req: &Request) -> Result<Response, Error>;
    fn stream(&self, req: &Request, cancel: &Arc<AtomicBool>) -> StreamHandle;
}

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [Tool],
}

#[derive(Debug)]
pub struct Response {
    pub message: Message,
    pub usage: Usage,
    pub stop_reason: String,
}

pub struct Agent<P: Provider> {
    provider: P,
    model: String,
    system: String,
    tools: Vec<Tool>,
    max_turns: usize,
    on: Option<Box<dyn FnMut(Event)>>,
}

impl<P: Provider> Agent<P> {
    pub fn new(provider: P) -> Self {
        Agent {
            provider,
            model: String::new(),
            system: String::new(),
            tools: Vec::new(),
            max_turns: usize::MAX,
            on: None,
        }
    }

    pub fn model(mut self, m: impl Into<String>) -> Self {
        self.model = m.into();
        self
    }

    pub fn system(mut self, s: impl Into<String>) -> Self {
        self.system = s.into();
        self
    }

    pub fn tools(mut self, ts: Vec<Tool>) -> Self {
        self.tools = ts;
        self
    }

    pub fn max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    pub fn on(mut self, f: impl FnMut(Event) + 'static) -> Self {
        self.on = Some(Box::new(f));
        self
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn run(&mut self, msgs: &[Message]) -> Result<Vec<Message>, Error> {
        let mut sink = AgentSink { on: self.on.take() };
        let end = run::run_stream(
            &self.provider,
            &run::RunOptions {
                model: &self.model,
                system: &self.system,
                tools: &self.tools,
                max_turns: self.max_turns,
            },
            msgs,
            &Arc::new(AtomicBool::new(false)),
            &mut sink,
        );
        self.on = sink.on;
        match end.outcome {
            run::Outcome::Done | run::Outcome::Cancelled | run::Outcome::Compact => {
                Ok(end.messages)
            }
            run::Outcome::MaxTurns => Err(Error::MaxTurns(end.messages)),
            run::Outcome::Failed(e) => Err(Error::Provider(e)),
        }
    }
}

struct AgentSink {
    on: Option<Box<dyn FnMut(Event)>>,
}

impl run::Sink for AgentSink {
    fn assistant(&mut self, turn: usize, msg: &Message, usage: Usage) {
        if let Some(f) = &mut self.on {
            f(Event {
                turn,
                message: msg.clone(),
                usage,
            });
        }
    }

    fn tool(&mut self, turn: usize, msg: &Message) {
        if let Some(f) = &mut self.on {
            f(Event {
                turn,
                message: msg.clone(),
                usage: Usage::default(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("axe-aw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        atomic_write(&path, b"world!").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world!");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert!(dir.join("f.txt").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn coerce_integer_accepts_float_strings() {
        let schema: Value = serde_json::from_str(r#"{"type":"integer"}"#).unwrap();
        let mut v = Value::String("3.5".into());
        coerce_value(&mut v, &schema);
        assert_eq!(v, Value::Number(serde_json::Number::from(3)));

        let mut v = Value::String("7".into());
        coerce_value(&mut v, &schema);
        assert_eq!(v, Value::Number(serde_json::Number::from(7)));
    }
}
