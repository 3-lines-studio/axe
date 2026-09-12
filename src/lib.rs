//! Minimal LLM coding agent harness.
//!
//! The loop is the only logic: messages -> LLM -> tool calls -> results ->
//! repeat. It lives in `run` and never mutates its input.
//!
//! Message/ToolCall serialize with PascalCase field names (session
//! storage); the OpenAI provider maps them to the wire format.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

pub mod app;
pub mod curlffi;
mod http;
pub mod image;
pub mod openai;
pub mod run;
pub mod session;
pub mod tools;
pub mod tui;

pub use openai::OpenAI;

/// Axe's built-in system prompt for a tool set and working directory, with the
/// user's `SYSTEM.md` appended when present. Shared so embedders use the exact
/// same prompt as the `axe` binary.
pub fn system_prompt(tools: &[Tool], dir: &str) -> String {
    let mut out = String::from(
        "You are an expert coding assistant operating inside axe. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n",
    );
    for t in tools {
        if !t.snippet.is_empty() {
            out.push_str(&format!("- {}: {}\n", t.name, t.snippet));
        }
    }
    out.push_str(
        "\nGuidelines:\n\
         - Be concise in your responses\n\
         - Show file paths clearly when working with files\n\
         - Use edit for precise changes; edits[].oldText must match exactly\n\
         - When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls\n\
         - Keep edits[].oldText small while still unique; do not pad with unchanged regions\n\
         - After a simple write or edit succeeds, stop unless the user asked you to verify it\n\
         - Tool errors return to you as text; fix them and re-issue\n",
    );
    out.push_str(&format!("\nCurrent working directory: {dir}"));
    if let Some(user) = user_system_prompt() {
        out.push_str("\n\n");
        out.push_str(&user);
    }
    out
}

fn user_system_prompt() -> Option<String> {
    let path = config_dir()?.join("axe").join("SYSTEM.md");
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// The axe config directory: `$XDG_CONFIG_HOME` or `$HOME/.config`.
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

/// Write via temp file + rename in the destination directory so a crash or
/// full disk never leaves a truncated file behind.
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

/// Temp-file name for one write. Unique across processes and across calls:
/// the destination directory can be shared (two axe instances on one project),
/// so the process id and the per-process call counter both take part. Without
/// the pid two processes could pick the same name at the same instant, and the
/// loser's error cleanup would unlink the winner's temp file. `create_new`
/// still guards the remaining sliver of pid reuse.
fn temp_name(pid: u32, secs: u64, nanos: u32, tag: u64) -> String {
    format!(".axe-tmp-{pid}-{}-{tag}", u64::from(nanos) ^ secs)
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
pub struct Image {
    #[serde(rename = "Path", default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(rename = "URL", default)]
    pub url: String,
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
    #[serde(
        rename = "Reasoning",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub reasoning: String,
    #[serde(rename = "Images", default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
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

#[derive(Debug)]
pub enum Error {
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
            Error::Transport(s) => write!(f, "{s}"),
            Error::Provider(s) => write!(f, "{s}"),
            Error::Http { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

/// What a tool hands back: text for the transcript, plus images the model
/// should see. Providers that support vision attach them to the tool result.
#[derive(Default)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<Image>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        ToolOutput::text(text)
    }
}

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
    pub run: Box<dyn Fn(&str, &mut dyn FnMut(&str)) -> ToolOutput + Send + Sync>,
}

pub fn new_tool<T, R>(
    name: &'static str,
    description: &'static str,
    schema: &'static str,
    run: impl Fn(T) -> R + Send + Sync + 'static,
) -> Tool
where
    T: DeserializeOwned,
    R: Into<ToolOutput>,
{
    new_tool_with_progress(name, description, schema, move |args, _progress| run(args))
}

/// Like `new_tool`, but the run closure also receives a progress callback it
/// can call with partial output while working (e.g. live bash output).
pub fn new_tool_with_progress<T, R>(
    name: &'static str,
    description: &'static str,
    schema: &'static str,
    run: impl Fn(T, &mut dyn FnMut(&str)) -> R + Send + Sync + 'static,
) -> Tool
where
    T: DeserializeOwned,
    R: Into<ToolOutput>,
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
                Ok(args) => run(args, progress).into(),
                Err(e) => ToolOutput::text(format!(
                    "error: invalid arguments for {name}: {e}\nReceived: {raw}"
                )),
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
    fn temp_name_distinguishes_processes_and_calls() {
        // Two processes writing the same directory at the same instant must not
        // pick the same temp file, or one's error cleanup unlinks the other's.
        assert_ne!(temp_name(1, 100, 7, 0), temp_name(2, 100, 7, 0));
        // Successive writes in one process must differ too.
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

    /// Concurrent processes targeting one destination must all succeed, and the
    /// file must end up as exactly one writer's payload: atomic_write never
    /// interleaves or truncates, and no writer's temp file is stolen.
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
