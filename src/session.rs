//! Per-project session store: live session file plus an archive catalog.
//! Live transcript: `{root}/session.jsonl`.
//! Archived sessions live in `{root}/sessions/<unix_ms>.jsonl`.
//!
//! Lines are append-only entries: a message, or a compaction summary with the
//! recent messages it retains. Older files with bare messages still load.

use crate::{Message, Provider, Request};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
    if !entries
        .iter()
        .any(|entry| matches!(entry, Entry::Compaction { .. }))
    {
        return entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
    }
    let original_task = original_task(entries);
    let workspace = workspace_state(entries);
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
                    content: format!(
                        "{COMPACTION_PREFIX}{summary}\n\n## Authoritative Original Task\n{original_task}\n\n## Authoritative Workspace State\n{workspace}{COMPACTION_SUFFIX}"
                    ),
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

fn original_task(entries: &[Entry]) -> &str {
    entries
        .iter()
        .find_map(|entry| match entry {
            Entry::Message { message }
                if message.role == "user" && !message.content.starts_with(COMPACTION_PREFIX) =>
            {
                Some(message.content.as_str())
            }
            _ => None,
        })
        .unwrap_or("")
}

#[derive(Deserialize)]
struct WorkspaceArgs<'a> {
    #[serde(borrow)]
    path: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    command: Option<std::borrow::Cow<'a, str>>,
}

fn workspace_state(entries: &[Entry]) -> String {
    let mut calls = BTreeMap::new();
    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut commands = VecDeque::with_capacity(8);
    for entry in entries {
        let Entry::Message { message } = entry else {
            continue;
        };
        if message.role == "assistant" {
            for call in &message.tool_calls {
                let arguments = serde_json::from_str::<WorkspaceArgs>(&call.arguments).ok();
                let path = arguments
                    .as_ref()
                    .and_then(|arguments| arguments.path.as_deref())
                    .map(str::to_string);
                let command = arguments
                    .as_ref()
                    .and_then(|arguments| arguments.command.as_deref())
                    .map(str::to_string);
                calls.insert(call.id.clone(), (call.name.clone(), path, command));
            }
            continue;
        }
        if message.role != "tool" {
            continue;
        }
        let failed = message.content.trim_start().starts_with("error:");
        let Some((name, path, command)) = calls.remove(&message.tool_call_id) else {
            continue;
        };
        if name == "bash" {
            if commands.len() == 8 {
                commands.pop_front();
            }
            commands.push_back((
                command.unwrap_or_else(|| "unknown command".into()),
                message.content.as_str(),
                failed,
            ));
            continue;
        }
        if failed {
            continue;
        }
        let Some(path) = path else {
            continue;
        };
        match name.as_str() {
            "read" => {
                read.insert(path);
            }
            "write" | "edit" => {
                modified.insert(path);
            }
            _ => {}
        }
    }
    let read = read.into_iter().collect::<Vec<_>>().join(", ");
    let modified = modified.into_iter().collect::<Vec<_>>().join(", ");
    let commands = commands
        .into_iter()
        .map(|(command, content, failed)| {
            let result = if !failed && content.trim().is_empty() {
                "success with no output".into()
            } else {
                compact_observation(content, 300)
            };
            format!("{command} => {result}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Files read: {}\nFiles modified: {}\nRecent commands and results:\n{}",
        if read.is_empty() { "none" } else { &read },
        if modified.is_empty() {
            "none"
        } else {
            &modified
        },
        if commands.is_empty() {
            "none"
        } else {
            &commands
        }
    )
}

fn parse_entry_line(line: &str) -> Option<Entry> {
    serde_json::from_str(line).ok()
}

fn read_entries(path: &Path) -> Vec<Entry> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| parse_entry_line(&line))
        .collect()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\\')
}

fn write_entries(path: &Path, entries: &[Entry]) -> std::io::Result<()> {
    crate::atomic_write_with(path, |file| {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(file);
        for entry in entries {
            serde_json::to_writer(&mut out, entry)?;
            out.write_all(b"\n")?;
        }
        out.flush()
    })
}

pub fn save_live(dir: &str, entries: &[Entry]) -> std::io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    write_entries(&live_path(dir), entries)
}

fn resume_id_path(dir: &str) -> PathBuf {
    Path::new(dir).join("session.resume_id")
}

pub fn set_resume_id(dir: &str, id: &str) {
    if !valid_id(id) {
        return;
    }
    let _ = crate::atomic_write(&resume_id_path(dir), id.as_bytes());
}

pub fn clear_resume_id(dir: &str) {
    let _ = std::fs::remove_file(resume_id_path(dir));
}

pub fn discard_live(dir: &str) {
    let _ = std::fs::remove_file(live_path(dir));
    let _ = std::fs::remove_file(Path::new(dir).join("session.title"));
    clear_resume_id(dir);
}

pub fn load_resume_id(dir: &str) -> Option<String> {
    let id = std::fs::read_to_string(resume_id_path(dir)).ok()?;
    let id = id.trim();
    if valid_id(id) {
        Some(id.to_string())
    } else {
        None
    }
}

/// Write a continued session back into its original archive instead of
/// forking a new one. Clears the live transcript so the next launch does
/// not re-archive it as a duplicate.
pub fn continue_archived(dir: &str, id: &str, entries: &[Entry]) -> std::io::Result<bool> {
    if !valid_id(id) || entries.is_empty() {
        return Ok(false);
    }
    write_entries(&store_dir(dir).join(format!("{id}.jsonl")), entries)?;
    let live_title = Path::new(dir).join("session.title");
    if let Ok(t) = std::fs::read_to_string(&live_title) {
        let t = t.trim();
        if !t.is_empty() {
            let _ = crate::atomic_write(&title_path(dir, id), t.as_bytes());
        }
    }
    let _ = std::fs::remove_file(&live_title);
    let _ = std::fs::remove_file(live_path(dir));
    clear_resume_id(dir);
    Ok(true)
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
        let (derived_title, turns) = session_summary(&path);
        let title = std::fs::read_to_string(title_path(dir, &id))
            .ok()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(derived_title);
        let meta = std::fs::metadata(&path).ok();
        let updated = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
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

#[derive(Deserialize)]
struct SummaryLine<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    message: Option<SummaryMessage<'a>>,
    #[serde(borrow)]
    summary: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    retained: Vec<SummaryMessage<'a>>,
}

#[derive(Deserialize)]
struct SummaryMessage<'a> {
    #[serde(borrow, alias = "Role")]
    role: std::borrow::Cow<'a, str>,
    #[serde(default, borrow, alias = "Content")]
    content: std::borrow::Cow<'a, str>,
}

fn session_summary(path: &Path) -> (String, usize) {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return ("Untitled session".into(), 0);
    };
    let mut title = String::new();
    let mut turns = 0;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<SummaryLine>(&line) else {
            continue;
        };
        match entry.kind.as_deref() {
            Some("message") => {
                let Some(message) = entry.message else {
                    continue;
                };
                if message.role != "user" {
                    continue;
                }
                if title.is_empty() && !message.content.is_empty() {
                    title = first_words(&message.content, 8);
                }
                turns += 1;
            }
            Some("compaction") => {
                let summary = entry.summary.unwrap_or_default();
                title = first_words(
                    &format!("{COMPACTION_PREFIX}{summary}{COMPACTION_SUFFIX}"),
                    8,
                );
                turns = 1 + entry
                    .retained
                    .iter()
                    .filter(|message| message.role == "user")
                    .count();
            }
            _ => {}
        }
    }
    if title.is_empty() {
        title = "Untitled session".into();
    }
    (title, turns)
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
    let entries = read_entries(&live_path(dir));
    if entries.is_empty() {
        return None;
    }
    if let Some(id) = load_resume_id(dir) {
        match continue_archived(dir, &id, &entries) {
            Ok(true) => return Some(id),
            Ok(false) => {}
            Err(_) => return None,
        }
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
    clear_resume_id(dir);
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

/// Rough token estimate for context budgeting: chars/4.
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
    let mut summarized = msgs;
    let retained = summarized.split_off(retained_start);
    (retained, summarized)
}

fn split_last_turn(mut msgs: Vec<Message>) -> (Vec<Message>, Vec<Message>) {
    let start = msgs
        .iter()
        .rposition(|message| message.role == "user")
        .unwrap_or(msgs.len());
    let retained = msgs.split_off(start);
    (retained, msgs)
}

fn serialize_conversation(msgs: &[Message]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut tools = BTreeMap::new();
    for m in msgs {
        match m.role.as_str() {
            "user" => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                let _ = write!(out, "[User]: {}", m.content);
            }
            "assistant" => {
                if !m.content.is_empty() {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    let _ = write!(out, "[Assistant]: {}", m.content);
                }
                for c in &m.tool_calls {
                    tools.insert(c.id.as_str(), c.name.as_str());
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    let _ = write!(out, "[Tool call {}]: {}({})", c.id, c.name, c.arguments);
                }
            }
            "tool" => {
                let limit = if tools.get(m.tool_call_id.as_str()) == Some(&"read") {
                    1200
                } else {
                    4000
                };
                let content = compact_observation(&m.content, limit);
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                let _ = write!(
                    out,
                    "[Tool result {}; full result remains in session]: {}",
                    m.tool_call_id, content
                );
            }
            _ => {}
        }
    }
    out
}

fn compact_observation(content: &str, limit: usize) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
    let half = limit / 2;
    let mut head_end = half;
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len() - half;
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n… [{} bytes masked] …\n{}",
        &content[..head_end],
        tail_start.saturating_sub(head_end),
        &content[tail_start..]
    )
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

fn fallback_summary(msgs: &[Message], candidate: &str) -> String {
    let mut facts = Vec::new();
    let mut size = 0;
    for message in msgs.iter().rev() {
        if message.content.is_empty() || !matches!(message.role.as_str(), "user" | "assistant") {
            continue;
        }
        let available = 6000usize.saturating_sub(size);
        if available < 200 {
            break;
        }
        let content = compact_observation(&message.content, available.min(1200));
        size += content.len();
        facts.push(format!("- {}: {content}", message.role));
    }
    facts.reverse();
    let candidate = candidate.trim();
    let candidate = if candidate.is_empty() {
        String::new()
    } else {
        format!(
            "\n- Model checkpoint: {}",
            compact_observation(candidate, 1200)
        )
    };
    format!(
        "## Goal\n- Continue the original task\n## User Requirements\n- See original task and retained messages\n## Progress\n### Done\n- See critical context\n### In Progress\n- Continue from the latest retained state\n### Blocked\n- Unknown\n## Key Decisions\n- See critical context\n## Files\n- See deterministic workspace state\n## Commands and Results\n- See critical context\n## Open Questions\n- Re-evaluate from retained state\n## Next Steps\n- Continue from the latest retained state\n## Critical Context\n{}{candidate}",
        facts.join("\n")
    )
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
    let first = request_summary(provider, model, &conversation, None).unwrap_or_default();
    let summary = if valid_summary(&first) {
        first
    } else {
        let second = request_summary(
            provider,
            model,
            &conversation,
            Some(
                "Your previous response was invalid. Return all required headings and substantive factual content.",
            ),
        )
        .unwrap_or_default();
        if valid_summary(&second) {
            second
        } else {
            fallback_summary(
                &to_summarize,
                if second.is_empty() { &first } else { &second },
            )
        }
    };
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
    fn session_summary_reads_minimal_fields() {
        let dir = std::env::temp_dir().join(format!("axe-summary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let entries = vec![
            Entry::Message {
                message: message("user", "quoted \" title"),
            },
            Entry::Message {
                message: message("assistant", "answer"),
            },
            Entry::Message {
                message: message("user", "again"),
            },
        ];
        write_entries(&path, &entries).unwrap();
        assert_eq!(session_summary(&path), ("quoted \" title".into(), 2));
        write_entries(
            &path,
            &[Entry::Compaction {
                summary: "summary".into(),
                tokens_before: 10,
                timestamp: 1,
                retained: vec![message("user", "recent")],
            }],
        )
        .unwrap();
        assert_eq!(session_summary(&path).1, 2);
        std::fs::remove_dir_all(dir).unwrap();
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

    #[test]
    fn compacted_context_keeps_original_task_and_workspace_state() {
        let mut read_call = message("assistant", "");
        read_call.tool_calls.push(crate::ToolCall {
            id: "read-1".into(),
            name: "read".into(),
            arguments: r#"{"path":"src/session.rs"}"#.into(),
        });
        let mut read_result = message("tool", "contents");
        read_result.tool_call_id = "read-1".into();
        let mut bash_call = message("assistant", "");
        bash_call.tool_calls.push(crate::ToolCall {
            id: "bash-1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        });
        let mut bash_result = message("tool", "all tests passed");
        bash_result.tool_call_id = "bash-1".into();
        let entries = vec![
            Entry::Message {
                message: message("user", "Fix compaction exactly"),
            },
            Entry::Message { message: read_call },
            Entry::Message {
                message: read_result,
            },
            Entry::Message { message: bash_call },
            Entry::Message {
                message: bash_result,
            },
            Entry::Compaction {
                summary: "summary".into(),
                tokens_before: 100,
                timestamp: 1,
                retained: Vec::new(),
            },
        ];
        let context = context_messages(&entries);
        assert!(context[0].content.contains("Fix compaction exactly"));
        assert!(context[0].content.contains("Files read: src/session.rs"));
        assert!(
            context[0]
                .content
                .contains("cargo test => all tests passed")
        );
    }

    #[test]
    fn old_read_observations_are_masked() {
        let mut call = message("assistant", "");
        call.tool_calls.push(crate::ToolCall {
            id: "read-1".into(),
            name: "read".into(),
            arguments: r#"{"path":"large"}"#.into(),
        });
        let mut result = message("tool", &"x".repeat(5000));
        result.tool_call_id = "read-1".into();
        let serialized = serialize_conversation(&[call, result]);
        assert!(serialized.contains("bytes masked"));
        assert!(serialized.len() < 2000);
    }

    #[test]
    fn archive_live_continues_resumed_session() {
        let dir = std::env::temp_dir().join(format!("axe-resume-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        save_live(
            d,
            &[Entry::Message {
                message: message("user", "first"),
            }],
        )
        .unwrap();
        let id = archive_live(d).expect("archive");
        let loaded = load_by_id(d, &id).unwrap();
        save_live(
            d,
            &[
                loaded[0].clone(),
                Entry::Message {
                    message: message("user", "second"),
                },
            ],
        )
        .unwrap();
        set_resume_id(d, &id);
        let again = archive_live(d).expect("continue");
        assert_eq!(again, id);
        assert_eq!(list_sessions(d).len(), 1);
        let msgs = context_messages(&load_by_id(d, &id).unwrap());
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].content, "second");
        assert!(!live_path(d).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discard_live_drops_transcript_and_resume_id() {
        let dir = std::env::temp_dir().join(format!("axe-discard-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        save_live(
            d,
            &[Entry::Message {
                message: message("user", "keep me not"),
            }],
        )
        .unwrap();
        set_resume_id(d, "abc");
        std::fs::write(Path::new(d).join("session.title"), "t").unwrap();
        discard_live(d);
        assert!(!live_path(d).exists());
        assert!(!resume_id_path(d).exists());
        assert!(!Path::new(d).join("session.title").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
