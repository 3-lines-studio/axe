use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

#[cfg(feature = "tui")]
pub mod app;
pub mod curlffi;
pub mod durable;
mod http;
pub mod ids;
pub mod image;
pub mod journal_store;
pub mod machine;
pub mod openai;
pub mod run;
pub mod sentinel;
pub mod session;
pub mod tools;
#[cfg(feature = "tui")]
pub mod tui;
pub mod web;

pub use ids::{EntryId, Seq, SessionId, TaskId};
pub use openai::OpenAI;

pub fn system_prompt(tools: &[Tool]) -> String {
    let mut out = String::from("Available tools:\n");
    for t in tools {
        if !t.snippet.is_empty() {
            out.push_str(&format!("- {}: {}\n", t.name, t.snippet));
        }
    }
    out
}

pub fn user_system_prompt() -> Option<String> {
    let path = config_dir()?.join("axe").join("SYSTEM.md");
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

pub fn config_dir() -> Option<std::path::PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
        && !x.is_empty()
    {
        return Some(std::path::PathBuf::from(x));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".config"))
}

#[cfg(target_os = "linux")]
pub fn set_non_dumpable() -> bool {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) == 0 }
}

#[cfg(not(target_os = "linux"))]
pub fn set_non_dumpable() -> bool {
    false
}

pub fn atomic_write(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    atomic_write_with(path, |file| file.write_all(data))
}

pub(crate) fn atomic_write_with(
    path: &std::path::Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TAG: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let parent = dir.unwrap_or(std::path::Path::new("."));
    let tag = TAG.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let tmp = parent.join(temp_name(
        std::process::id(),
        now.as_secs(),
        now.subsec_nanos(),
        tag,
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
            write(&mut file)?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path))
        .and_then(|()| std::fs::File::open(parent)?.sync_all());
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn temp_name(pid: u32, secs: u64, nanos: u32, tag: u64) -> String {
    format!(".axe-tmp-{pid}-{}-{tag}", u64::from(nanos) ^ secs)
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }
    };
}

string_id!(ToolCallId);
string_id!(ToolName);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "ID")]
    pub id: ToolCallId,
    #[serde(rename = "Name")]
    pub name: ToolName,
    #[serde(rename = "Arguments")]
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Image {
    pub path: String,
    pub url: String,
}

impl Serialize for Image {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let omit_url = !self.path.is_empty() && self.url.starts_with("data:");
        let mut state = serializer.serialize_struct("Image", if omit_url { 1 } else { 2 })?;
        if !self.path.is_empty() {
            state.serialize_field("Path", &self.path)?;
        }
        if !omit_url {
            state.serialize_field("URL", &self.url)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for Image {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(rename = "Path", default)]
            path: String,
            #[serde(rename = "URL", default)]
            url: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.url.is_empty()
            && !wire.path.is_empty()
            && let Some(image) = crate::image::attach_if_image(&wire.path)
        {
            return Ok(image);
        }
        Ok(Image {
            path: wire.path,
            url: wire.url,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl From<&str> for Role {
    fn from(value: &str) -> Self {
        match value {
            "system" => Role::System,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => Role::User,
        }
    }
}

impl From<String> for Role {
    fn from(value: String) -> Self {
        Role::from(value.as_str())
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for Role {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Role {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl Serialize for Role {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "system" => Ok(Role::System),
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            other => Err(serde::de::Error::custom(format!("unknown role: {other}"))),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "Role")]
    pub role: Role,
    #[serde(rename = "Content")]
    pub content: String,
    #[serde(rename = "ToolCalls", default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(
        rename = "ToolCallID",
        default,
        skip_serializing_if = "ToolCallId::is_empty"
    )]
    pub tool_call_id: ToolCallId,
    #[serde(
        rename = "Reasoning",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub reasoning: String,
    #[serde(rename = "Images", default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub context_window: Option<usize>,
    pub max_output: Option<usize>,
    pub tools: bool,
    pub images: bool,
    pub reasoning: bool,
    pub input_micros_per_million: Option<u64>,
    pub output_micros_per_million: Option<u64>,
}

impl ModelSpec {
    pub fn for_model(model: &str) -> Self {
        match model {
            "deepseek-flash" | "deepseek-v4-pro" => Self {
                id: model.to_string(),
                context_window: Some(1_000_000),
                max_output: Some(384_000),
                tools: true,
                images: model == "deepseek-flash",
                reasoning: true,
                input_micros_per_million: None,
                output_micros_per_million: None,
            },
            _ => Self {
                id: model.to_string(),
                context_window: None,
                max_output: None,
                tools: true,
                images: false,
                reasoning: false,
                input_micros_per_million: None,
                output_micros_per_million: None,
            },
        }
    }

    pub fn cost_micros(&self, usage: &Usage) -> Option<u64> {
        let input = self.input_micros_per_million?;
        let output = self.output_micros_per_million?;
        let input_cost = (usage.input as u128 * input as u128) / 1_000_000;
        let output_cost = ((usage.output + usage.reasoning) as u128 * output as u128) / 1_000_000;
        Some((input_cost + output_cost) as u64)
    }
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Message {
            role,
            content: content.into(),
            ..Message::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Message::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Message::new(Role::Assistant, content)
    }

    pub fn system(content: impl Into<String>) -> Self {
        Message::new(Role::System, content)
    }

    pub fn tool(call_id: impl Into<ToolCallId>, content: impl Into<String>) -> Self {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: call_id.into(),
            ..Message::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: usize,
    pub output: usize,
    pub cached_input: usize,
    pub reasoning: usize,
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

#[derive(Debug)]
pub enum Error {
    Transport(String),
    Provider(String),
    Http { status: u16, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Transport(s) => write!(f, "{s}"),
            Error::Provider(s) => write!(f, "{s}"),
            Error::Http { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug, Default)]
pub struct ToolContext {
    pub cancel: Option<Arc<AtomicBool>>,
}

impl ToolContext {
    pub fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    InvalidArguments(String),
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::InvalidArguments(message) => {
                write!(f, "invalid arguments: {message}")
            }
            ToolError::Cancelled => f.write_str("cancelled"),
            ToolError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ToolError {}

pub type ToolResult = Result<ToolOutput, ToolError>;

#[derive(Debug, Default)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<Image>,
    pub error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput {
            text: text.into(),
            images: Vec::new(),
            error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput {
            text: text.into(),
            images: Vec::new(),
            error: true,
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        if text.starts_with("error:") {
            ToolOutput::error(text)
        } else {
            ToolOutput::text(text)
        }
    }
}

impl From<ToolError> for ToolOutput {
    fn from(error: ToolError) -> Self {
        ToolOutput::error(format!("error: {error}"))
    }
}

pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub snippet: String,
    pub sequential: bool,
    #[allow(clippy::type_complexity)]
    pub run: Box<dyn Fn(&str, &ToolContext, &mut dyn FnMut(&str)) -> ToolResult + Send + Sync>,
}

pub fn new_tool<T, R>(
    name: impl Into<String>,
    description: impl Into<String>,
    schema: &'static str,
    run: impl Fn(T) -> R + Send + Sync + 'static,
) -> Tool
where
    T: DeserializeOwned,
    R: Into<ToolOutput>,
{
    new_tool_with_progress(name, description, schema, move |args, _progress| run(args))
}

pub fn new_tool_with_progress<T, R>(
    name: impl Into<String>,
    description: impl Into<String>,
    schema: &'static str,
    run: impl Fn(T, &mut dyn FnMut(&str)) -> R + Send + Sync + 'static,
) -> Tool
where
    T: DeserializeOwned,
    R: Into<ToolOutput>,
{
    let name = name.into();
    let parameters: Value = serde_json::from_str(schema).unwrap_or(Value::Null);
    let schema = parameters.clone();
    Tool {
        name: name.clone(),
        description: description.into(),
        parameters,
        snippet: String::new(),
        sequential: false,
        run: Box::new(move |raw, _context, progress| {
            let raw = if raw.trim().is_empty() { "{}" } else { raw };
            let mut args: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            if !args.is_null() {
                coerce_args(&mut args, &schema);
            }
            let coerced = serde_json::to_string(&args).unwrap_or_else(|_| raw.to_string());
            match serde_json::from_str::<T>(&coerced) {
                Ok(args) => Ok(run(args, progress).into()),
                Err(e) => Err(ToolError::InvalidArguments(format!(
                    "{name}: {e}\nReceived: {raw}"
                ))),
            }
        }),
    }
}

impl Tool {
    pub fn run_plain(&self, raw: &str, progress: &mut dyn FnMut(&str)) -> ToolOutput {
        match (self.run)(raw, &ToolContext::default(), progress) {
            Ok(output) => output,
            Err(error) => ToolOutput::from(error),
        }
    }
}

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
            if ty == Some("integer")
                && let Some(n) = value.as_f64()
                && n.is_finite()
                && n >= i64::MIN as f64
                && n <= i64::MAX as f64
            {
                *value = Value::Number(serde_json::Number::from(n as i64));
            } else if let Value::String(s) = value {
                if let Ok(n) = s.trim().parse::<f64>()
                    && n.is_finite()
                {
                    if ty == Some("integer") && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StopReason {
    #[default]
    End,
    Length,
    ToolCalls,
    ContentFilter,
    Interrupted,
    Other(String),
}

impl StopReason {
    pub fn from_wire(value: impl Into<String>) -> Self {
        match value.into().as_str() {
            "" | "stop" | "end" => StopReason::End,
            "length" => StopReason::Length,
            "tool_calls" | "function_call" => StopReason::ToolCalls,
            "content_filter" => StopReason::ContentFilter,
            "insufficient_system_resource" | "aborted" | "interrupted" => StopReason::Interrupted,
            other => StopReason::Other(other.to_string()),
        }
    }

    pub fn is_length(&self) -> bool {
        matches!(self, StopReason::Length)
    }

    pub fn is_interrupted(&self) -> bool {
        matches!(self, StopReason::Interrupted)
    }
}

impl From<String> for StopReason {
    fn from(value: String) -> Self {
        StopReason::from_wire(value)
    }
}

impl From<&str> for StopReason {
    fn from(value: &str) -> Self {
        StopReason::from_wire(value)
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::End => f.write_str("stop"),
            StopReason::Length => f.write_str("length"),
            StopReason::ToolCalls => f.write_str("tool_calls"),
            StopReason::ContentFilter => f.write_str("content_filter"),
            StopReason::Interrupted => f.write_str("interrupted"),
            StopReason::Other(value) => f.write_str(value),
        }
    }
}

impl PartialEq<str> for StopReason {
    fn eq(&self, other: &str) -> bool {
        match self {
            StopReason::End => other.is_empty() || other == "stop" || other == "end",
            StopReason::Length => other == "length",
            StopReason::ToolCalls => other == "tool_calls" || other == "function_call",
            StopReason::ContentFilter => other == "content_filter",
            StopReason::Interrupted => {
                matches!(
                    other,
                    "insufficient_system_resource" | "aborted" | "interrupted"
                )
            }
            StopReason::Other(value) => value == other,
        }
    }
}

impl PartialEq<&str> for StopReason {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
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
    pub stop_reason: StopReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn non_dumpable_helper() {
        assert!(set_non_dumpable());
        let pid = std::process::id();
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("cat /proc/{pid}/environ"))
            .output()
            .unwrap();
        let leaked = String::from_utf8_lossy(&out.stdout);
        assert!(
            !leaked.contains("AXE_SECRET_TEST_TOKEN"),
            "environ leaked: {leaked}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_dumpable_blocks_child_environ_read() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "tests::non_dumpable_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("AXE_SECRET_TEST_TOKEN", "leaky-value")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

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
        let target = dir.join("target.txt");
        let link = dir.join("link.txt");
        std::fs::write(&target, "target").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        atomic_write(&link, b"replacement").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "replacement");
        assert!(
            !std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore]
    fn atomic_write_crash_child() {
        use std::io::Write;
        let path = std::path::PathBuf::from(std::env::var_os("AXE_CRASH_PATH").unwrap());
        let marker = std::path::PathBuf::from(std::env::var_os("AXE_CRASH_MARKER").unwrap());
        atomic_write_with(&path, |file| {
            file.write_all(b"new")?;
            std::fs::write(&marker, b"ready")?;
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn atomic_write_survives_process_kill() {
        let dir = std::env::temp_dir().join(format!("axe-aw-kill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let marker = dir.join("ready");
        std::fs::write(&path, b"old").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::atomic_write_crash_child", "--ignored"])
            .env("AXE_CRASH_PATH", &path)
            .env("AXE_CRASH_MARKER", &marker)
            .spawn()
            .unwrap();
        for _ in 0..500 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(marker.exists());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".axe-tmp-"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roles_round_trip_as_wire_strings_and_reject_unknown_ones() {
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(
            serde_json::from_str::<Role>("\"tool\"").unwrap(),
            Role::Tool
        );
        assert!(serde_json::from_str::<Role>("\"nope\"").is_err());
        assert_eq!(Message::user("hi").role, Role::User);
    }

    #[test]
    fn model_specs_describe_deepseek_and_price_a_usage() {
        let flash = ModelSpec::for_model("deepseek-flash");
        assert_eq!(flash.context_window, Some(1_000_000));
        assert!(flash.images && flash.reasoning);

        let spec = ModelSpec {
            input_micros_per_million: Some(2),
            output_micros_per_million: Some(4),
            ..ModelSpec::for_model("deepseek-flash")
        };
        let usage = Usage {
            input: 1_000_000,
            output: 500_000,
            cached_input: 0,
            reasoning: 250_000,
        };
        assert_eq!(spec.cost_micros(&usage), Some(5));
    }

    #[test]
    fn temp_name_distinguishes_processes_and_calls() {
        assert_ne!(temp_name(1, 100, 7, 0), temp_name(2, 100, 7, 0));
        assert_ne!(temp_name(1, 100, 7, 0), temp_name(1, 100, 7, 1));
        assert_ne!(temp_name(1, 100, 7, 0), temp_name(1, 101, 7, 0));
        assert_ne!(temp_name(1, 100, 7, 0), temp_name(1, 100, 8, 0));
    }

    #[test]
    #[ignore]
    fn atomic_write_concurrent_child() {
        let path = std::path::PathBuf::from(std::env::var_os("AXE_CONCURRENT_PATH").unwrap());
        let payload = std::env::var("AXE_CONCURRENT_PAYLOAD").unwrap();
        atomic_write(&path, payload.as_bytes()).unwrap();
    }

    #[test]
    fn atomic_write_concurrent_processes() {
        let dir = std::env::temp_dir().join(format!("axe-aw-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let payloads: Vec<String> = (0..16)
            .map(|i| format!("writer-{i}-").repeat(500))
            .collect();
        let children: Vec<_> = payloads
            .iter()
            .map(|payload| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "tests::atomic_write_concurrent_child",
                        "--ignored",
                    ])
                    .env("AXE_CONCURRENT_PATH", &path)
                    .env("AXE_CONCURRENT_PAYLOAD", payload)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            assert!(
                child.wait().unwrap().success(),
                "a concurrent writer failed"
            );
        }
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            payloads.contains(&written),
            "destination is not any one writer's payload ({} bytes)",
            written.len()
        );
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

    #[test]
    fn coerce_integer_accepts_json_floats() {
        let schema: Value = serde_json::from_str(r#"{"type":"integer"}"#).unwrap();
        let mut v: Value = serde_json::from_str("430.0").unwrap();
        coerce_value(&mut v, &schema);
        assert_eq!(v, Value::Number(serde_json::Number::from(430)));

        let mut v: Value = serde_json::from_str("7.9").unwrap();
        coerce_value(&mut v, &schema);
        assert_eq!(v, Value::Number(serde_json::Number::from(7)));
    }
}
