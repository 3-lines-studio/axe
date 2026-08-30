//! Per-project session store: live session file plus an archive catalog.
//! Live transcript: `{root}/session.jsonl`.
//! Archived sessions live in `{root}/sessions/<unix_ms>.jsonl`.
//!
//! Lines are append-only entries: a message, or a compaction summary with the
//! recent messages it retains. Older files with bare messages still load.

use crate::{Message, Provider, Request};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Message {
        message: Message,
    },
    Compaction {
        summary: String,
        tokens_before: usize,
        timestamp: i64,
        retained: Vec<Message>,
    },
    Usage {
        input: usize,
        output: usize,
        cached_input: usize,
        context_input: usize,
        #[serde(default)]
        context_output: usize,
    },
}

#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub updated: i64,
    pub turns: usize,
    pub path: PathBuf,
}

fn store_dir(dir: &str) -> PathBuf {
    Path::new(dir).join("sessions")
}

pub fn scope_dir(dir: &str, cwd: &Path) -> String {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut hash = 0xcbf29ce484222325u64;
    for byte in cwd.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Path::new(dir)
        .join("projects")
        .join(format!("{hash:016x}"))
        .display()
        .to_string()
}

pub fn live_path(dir: &str) -> PathBuf {
    Path::new(dir).join("session.jsonl")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub const COMPACTION_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUFFIX: &str = "\n</summary>";

/// Project entries into the LLM context. A compaction entry supersedes every
/// message before it: the projection restarts from its summary plus the
/// recent messages it retains. The entry list itself is never rewritten.
pub fn context_messages(entries: &[Entry]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for e in entries {
        match e {
            Entry::Message { message } => out.push(message.clone()),
            Entry::Compaction {
                summary, retained, ..
            } => {
                out.clear();
                out.push(Message {
                    role: "user".into(),
                    content: format!("{COMPACTION_PREFIX}{summary}{COMPACTION_SUFFIX}"),
                    tool_calls: Vec::new(),
                    tool_call_id: String::new(),
                });
                out.extend(retained.iter().cloned());
            }
            Entry::Usage { .. } => {}
        }
    }
    out
}

fn parse_entry_line(line: &str) -> Option<Entry> {
    if let Ok(e) = serde_json::from_str::<Entry>(line) {
        return Some(e);
    }
    serde_json::from_str::<Message>(line)
        .ok()
        .map(|message| Entry::Message { message })
}

fn read_entries(path: &Path) -> Vec<Entry> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_entries(&data)
}

fn parse_entries(data: &str) -> Vec<Entry> {
    data.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse_entry_line)
        .collect()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\\')
}

fn write_entries(path: &Path, entries: &[Entry]) {
    let mut out = String::new();
    for e in entries {
        if let Ok(line) = serde_json::to_string(e) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    let _ = crate::atomic_write(path, out.as_bytes());
}

pub fn load_path(path: &Path) -> Result<Vec<Entry>, String> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read session {}: {e}", path.display())),
    };
    data.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            parse_entry_line(line).ok_or_else(|| format!("invalid session {}", path.display()))
        })
        .collect()
}

pub fn append_messages(path: &Path, messages: &[Message]) -> Result<(), String> {
    let mut entries = load_path(path)?;
    entries.extend(
        messages
            .iter()
            .cloned()
            .map(|message| Entry::Message { message }),
    );
    let mut out = String::new();
    for entry in entries {
        let line = serde_json::to_string(&entry)
            .map_err(|e| format!("encode session {}: {e}", path.display()))?;
        out.push_str(&line);
        out.push('\n');
    }
    crate::atomic_write(path, out.as_bytes())
        .map_err(|e| format!("write session {}: {e}", path.display()))
}

pub fn save_live(dir: &str, entries: &[Entry]) {
    if entries.is_empty() {
        return;
    }
    write_entries(&live_path(dir), entries);
}

/// Write a continued session back into its original archive instead of
/// forking a new one. A live title written by `/rename` during the resumed
/// session replaces the archived title. Clears the live transcript either
/// way so the next launch does not re-archive it as a duplicate.
pub fn continue_archived(dir: &str, id: &str, entries: &[Entry]) -> bool {
    if !valid_id(id) || entries.is_empty() {
        return false;
    }
    write_entries(&store_dir(dir).join(format!("{id}.jsonl")), entries);
    let live_title = Path::new(dir).join("session.title");
    if let Ok(t) = std::fs::read_to_string(&live_title) {
        let t = t.trim();
        if !t.is_empty() {
            let _ = crate::atomic_write(&title_path(dir, id), t.as_bytes());
        }
    }
    let _ = std::fs::remove_file(&live_title);
    let _ = std::fs::remove_file(live_path(dir));
    true
}

pub fn load_live(dir: &str) -> Vec<Entry> {
    read_entries(&live_path(dir))
}

pub fn list_sessions(dir: &str) -> Vec<SessionMeta> {
    let store = store_dir(dir);
    let Ok(entries) = std::fs::read_dir(&store) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let entries = read_entries(&path);
        let title = std::fs::read_to_string(title_path(dir, &id))
            .ok()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| title_from_entries(&entries));
        let meta = std::fs::metadata(&path).ok();
        let updated = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let turns = context_messages(&entries)
            .iter()
            .filter(|m| m.role == "user")
            .count();
        out.push(SessionMeta {
            id,
            title,
            updated,
            turns,
            path,
        });
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.updated));
    out
}

fn title_from_entries(entries: &[Entry]) -> String {
    for m in context_messages(entries) {
        if m.role == "user" && !m.content.is_empty() {
            return first_words(&m.content, 8);
        }
    }
    "Untitled session".into()
}

fn first_words(s: &str, n: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().take(n).collect();
    if words.is_empty() {
        return "Untitled session".into();
    }
    words.join(" ")
}

pub fn load_session(path: &Path) -> Vec<Entry> {
    read_entries(path)
}

pub fn archive_live(dir: &str) -> Option<String> {
    if read_entries(&live_path(dir)).is_empty() {
        return None;
    }
    let store = store_dir(dir);
    let _ = std::fs::create_dir_all(&store);
    let base = format!("{}", now_ms());
    let mut id = base.clone();
    let mut dest = store.join(format!("{id}.jsonl"));
    let mut n = 1;
    while dest.exists() {
        id = format!("{base}-{n}");
        dest = store.join(format!("{id}.jsonl"));
        n += 1;
    }
    if std::fs::copy(live_path(dir), &dest).is_err() {
        return None;
    }
    let live_title_path = Path::new(dir).join("session.title");
    let title = std::fs::read_to_string(&live_title_path)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| title_from_entries(&read_entries(&dest)));
    let _ = crate::atomic_write(&title_path(dir, &id), title.as_bytes());
    let _ = std::fs::remove_file(live_title_path);
    let _ = std::fs::remove_file(live_path(dir));
    Some(id)
}

pub fn load_by_id(dir: &str, id: &str) -> Option<Vec<Entry>> {
    if !valid_id(id) {
        return None;
    }
    let path = store_dir(dir).join(format!("{id}.jsonl"));
    if !path.exists() {
        return None;
    }
    Some(read_entries(&path))
}

fn title_path(dir: &str, id: &str) -> PathBuf {
    store_dir(dir).join(format!("{id}.title"))
}

pub fn set_live_title(dir: &str, title: &str) {
    let path = Path::new(dir).join("session.title");
    let _ = crate::atomic_write(&path, title.as_bytes());
}

/// Rough token estimate for context budgeting: chars/4.
pub fn estimate_tokens(msgs: &[Message]) -> usize {
    let mut chars = 0usize;
    for m in msgs {
        chars += m.content.len();
        for c in &m.tool_calls {
            chars += c.name.len() + c.arguments.len();
        }
    }
    chars / 4
}

/// Ensure the transcript does not end with an unanswered tool-call exchange:
/// providers reject an assistant message whose tool_calls lack matching tool
/// results. Drops such an exchange (e.g. from a crash mid-batch in an older
/// session); complete exchanges are kept.
pub fn trim_trailing_tool_messages(msgs: &mut Vec<Message>) {
    let Some(pos) = msgs
        .iter()
        .rposition(|m| m.role == "assistant" && !m.tool_calls.is_empty())
    else {
        return;
    };
    let answered = msgs[pos + 1..].iter().filter(|m| m.role == "tool").count();
    if answered < msgs[pos].tool_calls.len() {
        msgs.truncate(pos);
    }
}

const RETAIN_TOKENS: usize = 20_000;

pub fn latest_context_tokens(entries: &[Entry]) -> Option<usize> {
    for entry in entries.iter().rev() {
        match entry {
            Entry::Usage {
                context_input,
                context_output,
                ..
            } if *context_input > 0 => {
                return Some(context_input.saturating_add(*context_output));
            }
            Entry::Compaction { .. } => return None,
            _ => {}
        }
    }
    None
}

fn split_retained(entries: &[Entry]) -> (Vec<Message>, Vec<Message>) {
    let msgs = context_messages(entries);
    let Some(current_tokens) = latest_context_tokens(entries) else {
        return split_last_turn(msgs);
    };
    let mut message_count = msgs.len();
    let mut retained_start = message_count;
    let active_start = entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction { .. }))
        .map(|index| index + 1)
        .unwrap_or(0);
    for entry in entries[active_start..].iter().rev() {
        match entry {
            Entry::Message { .. } => {
                message_count = message_count.saturating_sub(1);
            }
            Entry::Usage {
                context_input,
                context_output,
                ..
            } if *context_input > 0 => {
                let boundary_tokens = context_input.saturating_add(*context_output);
                if current_tokens.saturating_sub(boundary_tokens) > RETAIN_TOKENS {
                    break;
                }
                retained_start = message_count;
            }
            _ => {}
        }
    }
    if retained_start == msgs.len() {
        return split_last_turn(msgs);
    }
    (
        msgs[retained_start..].to_vec(),
        msgs[..retained_start].to_vec(),
    )
}

fn split_last_turn(msgs: Vec<Message>) -> (Vec<Message>, Vec<Message>) {
    let start = msgs
        .iter()
        .rposition(|message| message.role == "user")
        .unwrap_or(msgs.len());
    (msgs[start..].to_vec(), msgs[..start].to_vec())
}

fn serialize_conversation(msgs: &[Message]) -> String {
    let mut parts = Vec::new();
    for m in msgs {
        match m.role.as_str() {
            "user" => parts.push(format!("[User]: {}", m.content)),
            "assistant" => {
                if !m.content.is_empty() {
                    parts.push(format!("[Assistant]: {}", m.content));
                }
                for c in &m.tool_calls {
                    parts.push(format!("[Tool call]: {}({})", c.name, c.arguments));
                }
            }
            "tool" => {
                let content = if m.content.len() > 4000 {
                    let mut head_end = 2000;
                    while !m.content.is_char_boundary(head_end) {
                        head_end -= 1;
                    }
                    let mut tail_start = m.content.len() - 2000;
                    while !m.content.is_char_boundary(tail_start) {
                        tail_start += 1;
                    }
                    format!(
                        "{}\n… [middle truncated] …\n{}",
                        &m.content[..head_end],
                        &m.content[tail_start..]
                    )
                } else {
                    m.content.clone()
                };
                parts.push(format!("[Tool result for {}]: {}", m.tool_call_id, content));
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

const SUMMARY_SYSTEM: &str = "You are a context summarization assistant. Read the conversation and produce a structured summary so another LLM can continue the work. Do NOT continue the conversation. Do NOT respond to questions in it. ONLY output the summary.";

const SUMMARY_PROMPT: &str = "Create a factual context checkpoint that another coding agent will use to continue the work.

Use these exact headings:

## Goal
## User Requirements
## Progress
### Done
### In Progress
### Blocked
## Key Decisions
## Files
## Commands and Results
## Open Questions
## Next Steps
## Critical Context

Use concise bullets. Record only facts supported by the conversation. Distinguish completed work from proposed work. Preserve exact file paths, symbol names, commands, exit status, error messages, values, and user requirements. Keep still-active facts from an earlier checkpoint. Remove superseded facts. Do not copy large tool outputs or source files; retain only details needed to continue.";

const SUMMARY_HEADINGS: [&str; 12] = [
    "## Goal",
    "## User Requirements",
    "## Progress",
    "### Done",
    "### In Progress",
    "### Blocked",
    "## Key Decisions",
    "## Files",
    "## Commands and Results",
    "## Open Questions",
    "## Next Steps",
    "## Critical Context",
];

fn valid_summary(summary: &str) -> bool {
    let summary = summary.trim();
    summary.len() >= 100
        && SUMMARY_HEADINGS
            .iter()
            .all(|heading| summary.contains(heading))
}

fn request_summary(
    provider: &impl Provider,
    model: &str,
    conversation: &str,
    correction: Option<&str>,
) -> Result<String, String> {
    let correction = correction.unwrap_or("");
    let prompt = format!(
        "<conversation>\n{conversation}\n</conversation>\n\n{SUMMARY_PROMPT}\n\n{correction}"
    );
    let message = Message {
        role: "user".into(),
        content: prompt,
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    };
    let req = Request {
        model,
        system: SUMMARY_SYSTEM,
        messages: std::slice::from_ref(&message),
        tools: &[],
    };
    provider
        .complete(&req)
        .map(|response| response.message.content.trim().to_string())
        .map_err(|error| error.to_string())
}

/// Summarize the conversation in `entries`, returning the summary text, the
/// provider-reported context tokens before compaction, and retained recent messages.
pub fn compact(
    provider: &impl Provider,
    model: &str,
    entries: &[Entry],
) -> Result<(String, usize, Vec<Message>), String> {
    let tokens_before = latest_context_tokens(entries).unwrap_or(0);
    let (retained, to_summarize) = split_retained(entries);
    if to_summarize.is_empty() {
        return Err("nothing to summarize".into());
    }
    let conversation = serialize_conversation(&to_summarize);
    let mut summary = request_summary(provider, model, &conversation, None)?;
    if !valid_summary(&summary) {
        summary = request_summary(
            provider,
            model,
            &conversation,
            Some(
                "Your previous response was invalid. Return all required headings and substantive factual content.",
            ),
        )?;
    }
    if !valid_summary(&summary) {
        return Err("invalid compaction summary".into());
    }
    Ok((summary, tokens_before, retained))
}

/// Provider error messages that indicate the context window was exceeded.
const OVERFLOW_PATTERNS: [&str; 7] = [
    "prompt is too long",
    "exceeds the context window",
    "maximum context length",
    "input token count",
    "context_length_exceeded",
    "prompt too long",
    "exceeds the model's maximum",
];

/// Errors that look like overflow but are throttling or server failures.
const NON_OVERFLOW_PATTERNS: [&str; 3] = ["throttling", "rate limit", "service unavailable"];

pub fn is_overflow_error(err: &str) -> bool {
    let e = err.to_lowercase();
    if NON_OVERFLOW_PATTERNS.iter().any(|p| e.contains(p)) {
        return false;
    }
    OVERFLOW_PATTERNS.iter().any(|p| e.contains(p))
}

#[derive(Debug)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub updated: i64,
    pub text: String,
}

fn clip(s: &str, n: usize) -> String {
    let count = s.chars().count();
    let mut out: String = s.chars().take(n).collect();
    if count > n {
        out.push('…');
    }
    out
}

/// Case-insensitive substring search over the live session and archived
/// sessions. JSONL is one entry per line, so line matches map to entries.
pub fn search(dir: &str, text: &str) -> Vec<SearchHit> {
    let needle = text.to_lowercase();
    let mut out = Vec::new();
    let mut scan = |path: &Path, id: &str| {
        let Ok(data) = std::fs::read_to_string(path) else {
            return;
        };
        let entries = parse_entries(&data);
        let title = std::fs::read_to_string(title_path(dir, id))
            .ok()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| title_from_entries(&entries));
        let updated = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        for line in data.lines() {
            let parsed = parse_entry_line(line);
            if matches!(parsed, Some(Entry::Usage { .. })) {
                continue;
            }
            if !line.to_lowercase().contains(&needle) {
                continue;
            }
            let text = parsed
                .map(|e| match e {
                    Entry::Message { message } => message.content,
                    Entry::Compaction { summary, .. } => summary,
                    Entry::Usage { .. } => unreachable!(),
                })
                .unwrap_or_else(|| line.to_string());
            out.push(SearchHit {
                id: id.into(),
                title: title.clone(),
                updated,
                text: clip(&text, 200),
            });
            if out.len() >= 50 {
                return;
            }
        }
    };
    let live = live_path(dir);
    if live.exists() {
        scan(&live, "live");
    }
    if let Ok(entries) = std::fs::read_dir(store_dir(dir)) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            scan(&path, &id);
        }
    }
    out.sort_by_key(|h| std::cmp::Reverse(h.updated));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
        }
    }

    fn usage(context_input: usize, context_output: usize) -> Entry {
        Entry::Usage {
            input: context_input,
            output: context_output,
            cached_input: 0,
            context_input,
            context_output,
        }
    }

    #[test]
    fn retained_context_uses_usage_and_complete_turns() {
        let mut tool_call = message("assistant", "");
        tool_call.tool_calls.push(crate::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: "{\"path\":\"src/main.rs\"}".into(),
        });
        let mut tool_result = message("tool", "file contents");
        tool_result.tool_call_id = "call-1".into();
        let entries = vec![
            Entry::Message {
                message: message("user", "old"),
            },
            Entry::Message {
                message: message("assistant", "old answer"),
            },
            usage(100_000, 100),
            Entry::Message {
                message: message("user", "middle"),
            },
            Entry::Message {
                message: message("assistant", "middle answer"),
            },
            usage(110_000, 100),
            Entry::Message {
                message: message("user", "latest"),
            },
            Entry::Message { message: tool_call },
            Entry::Message {
                message: tool_result,
            },
            Entry::Message {
                message: message("assistant", "latest answer"),
            },
            usage(125_000, 100),
        ];
        let (retained, summarized) = split_retained(&entries);
        assert_eq!(latest_context_tokens(&entries), Some(125_100));
        assert_eq!(retained.len(), 4);
        assert_eq!(retained[0].content, "latest");
        assert_eq!(retained[1].tool_calls[0].id, "call-1");
        assert_eq!(retained[2].tool_call_id, "call-1");
        assert_eq!(summarized.last().unwrap().content, "middle answer");
    }

    #[test]
    fn compaction_resets_persisted_context_usage() {
        let entries = vec![
            usage(250_000, 500),
            Entry::Compaction {
                summary: "summary".into(),
                tokens_before: 250_500,
                timestamp: 1,
                retained: Vec::new(),
            },
        ];
        assert_eq!(latest_context_tokens(&entries), None);
    }
}
