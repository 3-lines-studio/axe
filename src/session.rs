use crate::ids::{EntryId, SessionId};
use crate::{Message, Provider, Request, Usage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Message {
        message: Message,
    },
    Reset,
    Compaction {
        summary: String,
        tokens_before: usize,
        timestamp: i64,
        retained: Vec<Message>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: EntryId,
    pub at: i64,
    pub session: SessionId,
    pub entry: Entry,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPoint {
    pub index: usize,
    pub tokens: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub input: usize,
    pub output: usize,
    pub cached_input: usize,
    pub reasoning: usize,
    pub context_input: usize,
    pub context_output: usize,
    #[serde(default)]
    pub points: Vec<ContextPoint>,
}

impl SessionState {
    pub fn record(&mut self, usage: &Usage, context: &Usage) {
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.cached_input = self.cached_input.saturating_add(usage.cached_input);
        self.reasoning = self.reasoning.saturating_add(usage.reasoning);
        self.context_input = context.input;
        self.context_output = context.output;
    }

    pub fn context_tokens(&self) -> usize {
        self.context_input.saturating_add(self.context_output)
    }

    pub fn point(&mut self, index: usize, tokens: usize) {
        self.points.push(ContextPoint { index, tokens });
    }
}

#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub updated: i64,
    pub turns: usize,
}

pub trait Store: Send + Sync {
    fn live(&self) -> Vec<Entry>;

    fn save(&self, entries: &[Entry]) -> std::io::Result<()>;

    fn append(&self, entries: &[Entry]) -> std::io::Result<()>;

    fn archive(&self) -> Option<String>;

    fn continue_archived(&self, id: &str, entries: &[Entry]) -> std::io::Result<bool>;

    fn continue_archived_live(&self, id: &str) -> std::io::Result<bool>;

    fn discard(&self);

    fn list(&self) -> Vec<SessionMeta>;

    fn load(&self, id: &str) -> Option<Vec<Entry>>;

    fn resume_id(&self) -> Option<String>;
    fn set_resume_id(&self, id: &str);
    fn clear_resume_id(&self);

    fn state(&self) -> SessionState {
        SessionState::default()
    }

    fn append_with_state(&self, entries: &[Entry], _state: SessionState) -> std::io::Result<()> {
        self.append(entries)
    }

    fn save_with_state(&self, entries: &[Entry], _state: SessionState) -> std::io::Result<()> {
        self.save(entries)
    }

    fn continue_archived_with_state(
        &self,
        id: &str,
        entries: &[Entry],
        _state: SessionState,
    ) -> std::io::Result<bool> {
        self.continue_archived(id, entries)
    }
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

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub const COMPACTION_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUFFIX: &str = "\n</summary>";

#[derive(Clone, Copy)]
pub struct ContextOptions {
    pub original_task: bool,
    pub workspace_state: bool,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            original_task: true,
            workspace_state: true,
        }
    }
}

pub fn context_messages(entries: &[Entry]) -> Vec<Message> {
    context_messages_with(entries, ContextOptions::default())
}

pub fn context_messages_with(entries: &[Entry], options: ContextOptions) -> Vec<Message> {
    let entries = after_reset(entries);
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
    let mut sections = Vec::new();
    if options.original_task {
        sections.push(format!(
            "## Authoritative Original Task\n{}",
            original_task(entries)
        ));
    }
    if options.workspace_state {
        sections.push(format!(
            "## Authoritative Workspace State\n{}",
            workspace_state(entries)
        ));
    }
    let extras = sections.join("\n\n");
    let mut out: Vec<Message> = Vec::new();
    for e in entries {
        match e {
            Entry::Message { message } => out.push(message.clone()),
            Entry::Compaction {
                summary, retained, ..
            } => {
                let content = if extras.is_empty() {
                    format!("{COMPACTION_PREFIX}{summary}{COMPACTION_SUFFIX}")
                } else {
                    format!("{COMPACTION_PREFIX}{summary}\n\n{extras}{COMPACTION_SUFFIX}")
                };
                out.clear();
                out.push(Message {
                    role: "user".into(),
                    content,
                    tool_calls: Vec::new(),
                    tool_call_id: Default::default(),
                    reasoning: String::new(),
                    images: Vec::new(),
                });
                out.extend(retained.iter().cloned());
            }
            Entry::Reset => out.clear(),
        }
    }
    out
}

pub(crate) fn after_reset(entries: &[Entry]) -> &[Entry] {
    let start = entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Reset))
        .map(|at| at + 1)
        .unwrap_or(0);
    &entries[start..]
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
    let mut calls = HashMap::new();
    let mut read = Vec::new();
    let mut modified = Vec::new();
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
            "read" => read.push(path),
            "write" | "edit" => modified.push(path),
            _ => {}
        }
    }
    read.sort_unstable();
    read.dedup();
    modified.sort_unstable();
    modified.dedup();
    let read = read.join(", ");
    let modified = modified.join(", ");
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

pub(crate) fn summarize(records: &[Record]) -> (String, usize, i64) {
    let entries: Vec<Entry> = records.iter().map(|record| record.entry.clone()).collect();
    let title = title_from_entries(&entries);
    let turns = entry_turns(&entries);
    let updated = records.iter().map(|record| record.at).max().unwrap_or(0);
    (title, turns, updated)
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

fn next_turns(turns: usize, entry: &Entry) -> usize {
    match entry {
        Entry::Message { message } if message.role == "user" => turns + 1,
        Entry::Compaction { retained, .. } => {
            1 + retained
                .iter()
                .filter(|message| message.role == "user")
                .count()
        }
        Entry::Reset => 0,
        _ => turns,
    }
}

fn entry_turns(entries: &[Entry]) -> usize {
    entries.iter().fold(0, next_turns)
}

pub fn drop_incomplete_tool_calls(msgs: &mut Vec<Message>) {
    let mut results: HashMap<&str, usize> = HashMap::new();
    for (index, message) in msgs.iter().enumerate() {
        if message.role == "tool" {
            results
                .entry(message.tool_call_id.as_str())
                .or_insert(index);
        }
    }
    let mut used = vec![false; msgs.len()];
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    for (index, message) in msgs.iter().enumerate() {
        if used[index] || message.role == "tool" {
            continue;
        }
        if message.role != "assistant" || message.tool_calls.is_empty() {
            out.push(message.clone());
            continue;
        }
        let ids: Vec<&str> = message
            .tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect();
        if !ids
            .iter()
            .all(|id| results.get(id).is_some_and(|index| !used[*index]))
        {
            continue;
        }
        out.push(message.clone());
        for id in ids {
            let index = results[id];
            used[index] = true;
            out.push(msgs[index].clone());
        }
    }
    *msgs = out;
}

const RETAIN_TOKENS: usize = 20_000;

fn after_reset_start(entries: &[Entry]) -> usize {
    entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Reset))
        .map(|at| at + 1)
        .unwrap_or(0)
}

fn split_retained_state(
    entries: &[Entry],
    options: ContextOptions,
    state: &SessionState,
) -> (Vec<Message>, Vec<Message>) {
    let start = after_reset_start(entries);
    let entries = &entries[start..];
    let msgs = context_messages_with(entries, options);
    let current_tokens = state.context_tokens();
    if current_tokens == 0 {
        return split_last_turn(msgs);
    }
    let mut points = BTreeMap::new();
    for point in &state.points {
        if point.index >= start {
            points.insert(point.index - start, point.tokens);
        }
    }
    let mut message_count = msgs.len();
    let mut retained_start = message_count;
    let active_start = entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction { .. }))
        .map(|index| index + 1)
        .unwrap_or(0);
    for (index, entry) in entries.iter().enumerate().rev() {
        if index < active_start {
            break;
        }
        if let Entry::Message { .. } = entry {
            message_count = message_count.saturating_sub(1);
        }
        if let Some(tokens) = points.get(&index) {
            if current_tokens.saturating_sub(*tokens) > RETAIN_TOKENS {
                break;
            }
            retained_start = message_count;
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
        tool_call_id: Default::default(),
        reasoning: String::new(),
        images: Vec::new(),
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

pub fn compact_with_state(
    provider: &impl Provider,
    model: &str,
    entries: &[Entry],
    state: &SessionState,
    options: ContextOptions,
) -> Result<(String, usize, Vec<Message>), String> {
    let tokens_before = state.context_tokens();
    let (mut retained, to_summarize) = split_retained_state(entries, options, state);
    drop_incomplete_tool_calls(&mut retained);
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

const OVERFLOW_PATTERNS: [&str; 7] = [
    "prompt is too long",
    "exceeds the context window",
    "maximum context length",
    "input token count",
    "context_length_exceeded",
    "prompt too long",
    "exceeds the model's maximum",
];

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
            tool_call_id: Default::default(),
            reasoning: String::new(),
            images: Vec::new(),
        }
    }

    fn call(id: &str) -> crate::ToolCall {
        crate::ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{}".into(),
        }
    }

    fn tool_result(id: &str) -> Message {
        let mut message = message("tool", "output");
        message.tool_call_id = id.into();
        message
    }

    #[test]
    fn a_reset_drops_the_context_before_it() {
        let entries = vec![
            Entry::Message {
                message: message("user", "old"),
            },
            Entry::Message {
                message: message("assistant", "old answer"),
            },
            Entry::Reset,
            Entry::Message {
                message: message("user", "new"),
            },
        ];
        let messages = context_messages(&entries);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "new");
    }

    #[test]
    fn a_reset_restarts_the_turn_count() {
        assert_eq!(next_turns(4, &Entry::Reset), 0);
        assert_eq!(
            next_turns(
                0,
                &Entry::Message {
                    message: message("user", "new"),
                }
            ),
            1
        );
    }

    #[test]
    fn a_compaction_after_a_reset_summarizes_only_what_follows() {
        let mut entries = vec![
            Entry::Message {
                message: message("user", "forgotten"),
            },
            Entry::Reset,
            Entry::Message {
                message: message("user", "kept"),
            },
        ];
        entries.push(Entry::Compaction {
            summary: "summary".into(),
            tokens_before: 11,
            timestamp: 0,
            retained: Vec::new(),
        });
        let messages = context_messages(&entries);
        assert_eq!(messages[0].role, "user");
        assert!(messages[0].content.contains("summary"));
        assert!(!messages[0].content.contains("forgotten"));
    }

    #[test]
    fn retained_context_uses_token_points_and_complete_turns() {
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
            Entry::Message {
                message: message("user", "middle"),
            },
            Entry::Message {
                message: message("assistant", "middle answer"),
            },
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
        ];
        let state = SessionState {
            context_input: 125_000,
            context_output: 100,
            points: vec![
                ContextPoint {
                    index: 1,
                    tokens: 100_100,
                },
                ContextPoint {
                    index: 3,
                    tokens: 110_100,
                },
                ContextPoint {
                    index: 7,
                    tokens: 125_100,
                },
            ],
            ..SessionState::default()
        };
        let (retained, summarized) =
            split_retained_state(&entries, ContextOptions::default(), &state);
        assert_eq!(state.context_tokens(), 125_100);
        assert_eq!(retained.len(), 5);
        assert_eq!(retained[0].content, "middle answer");
        assert_eq!(retained[1].content, "latest");
        assert_eq!(retained[2].tool_calls[0].id, "call-1");
        assert_eq!(retained[3].tool_call_id, "call-1");
        assert_eq!(summarized.last().unwrap().content, "middle");
    }

    struct SummaryProvider;

    impl crate::Provider for SummaryProvider {
        fn complete(&self, _req: &Request) -> Result<crate::Response, crate::Error> {
            let mut content = String::new();
            for heading in SUMMARY_HEADINGS {
                content.push_str(heading);
                content.push_str("\n- fact\n");
            }
            Ok(crate::Response {
                message: message("assistant", &content),
                usage: crate::Usage::default(),
                stop_reason: "stop".into(),
            })
        }

        fn stream(
            &self,
            _req: &Request,
            _cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> crate::StreamHandle {
            unimplemented!()
        }
    }

    #[test]
    fn compact_drops_incomplete_tool_calls_from_retained() {
        let mut tool_call = message("assistant", "");
        tool_call.tool_calls.push(crate::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: "{\"path\":\"src/main.rs\"}".into(),
        });
        tool_call.tool_calls.push(crate::ToolCall {
            id: "call-2".into(),
            name: "read".into(),
            arguments: "{\"path\":\"src/lib.rs\"}".into(),
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
            Entry::Message {
                message: message("user", "latest"),
            },
            Entry::Message { message: tool_call },
            Entry::Message {
                message: tool_result,
            },
        ];
        let (_, _, retained) = compact_with_state(
            &SummaryProvider,
            "test",
            &entries,
            &SessionState::default(),
            ContextOptions::default(),
        )
        .unwrap();
        assert_eq!(retained[0].content, "latest");
        assert!(!retained.iter().any(|message| message.content == "old"));
    }

    #[test]
    fn out_of_order_tool_results_go_back_with_their_calls() {
        let mut first = message("assistant", "");
        first.tool_calls.push(call("call-1"));
        let mut second = message("assistant", "");
        second.tool_calls.push(call("call-2"));
        let mut msgs = vec![
            message("user", "go"),
            first,
            second,
            tool_result("call-2"),
            message("assistant", "done"),
            tool_result("call-1"),
        ];
        drop_incomplete_tool_calls(&mut msgs);
        let shape: Vec<(&str, &str)> = msgs
            .iter()
            .map(|m| (m.role.as_str(), m.tool_call_id.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("user", ""),
                ("assistant", ""),
                ("tool", "call-1"),
                ("assistant", ""),
                ("tool", "call-2"),
                ("assistant", ""),
            ]
        );
    }

    #[test]
    fn a_tool_result_without_its_call_is_dropped() {
        let mut msgs = vec![
            message("user", "go"),
            message("assistant", "answer"),
            tool_result("call-1"),
        ];
        drop_incomplete_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 2);
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
    fn context_options_drop_original_task_and_workspace_state() {
        let entries = vec![
            Entry::Message {
                message: message("user", "Fix compaction exactly"),
            },
            Entry::Message {
                message: message("assistant", "done"),
            },
            Entry::Compaction {
                summary: "summary".into(),
                tokens_before: 100,
                timestamp: 1,
                retained: Vec::new(),
            },
        ];
        let options = ContextOptions {
            original_task: false,
            workspace_state: false,
        };
        let context = context_messages_with(&entries, options);
        assert!(context[0].content.contains("summary"));
        assert!(!context[0].content.contains("Authoritative Original Task"));
        assert!(!context[0].content.contains("Authoritative Workspace State"));
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
}
