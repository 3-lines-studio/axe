use crate::session::{self, Entry, Store};
use crate::{Message, ToolCall};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    Picker,
    Session(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    New,
    Resume,
    Rewind,
    Compact,
    Copy,
    Image,
    Skill,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: &'static str,
    pub help: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub action: Command,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: "/help",
        help: "/help",
        description: "show available slash commands",
        category: "General",
        action: Command::Help,
    },
    CommandSpec {
        command: "/new",
        help: "/new",
        description: "start a fresh session",
        category: "Session",
        action: Command::New,
    },
    CommandSpec {
        command: "/resume",
        help: "/resume",
        description: "resume a saved session",
        category: "Session",
        action: Command::Resume,
    },
    CommandSpec {
        command: "/rewind",
        help: "/rewind",
        description: "rewind the session to an earlier message",
        category: "Session",
        action: Command::Rewind,
    },
    CommandSpec {
        command: "/compact",
        help: "/compact",
        description: "summarize the conversation so far",
        category: "Session",
        action: Command::Compact,
    },
    CommandSpec {
        command: "/copy",
        help: "/copy",
        description: "copy the last assistant response",
        category: "Session",
        action: Command::Copy,
    },
    CommandSpec {
        command: "/image",
        help: "/image PATH",
        description: "attach an image (file path or URL) to the next message",
        category: "Session",
        action: Command::Image,
    },
    CommandSpec {
        command: "/skill",
        help: "/skill NAME",
        description: "load a skill's instructions and run them",
        category: "Skills",
        action: Command::Skill,
    },
    CommandSpec {
        command: "/quit",
        help: "/quit",
        description: "exit the interactive shell",
        category: "General",
        action: Command::Quit,
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindItem {
    pub message_index: usize,
    pub role: String,
    pub preview: String,
}

pub fn parse_command(input: &str) -> Option<Command> {
    let name = input
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or_default();
    COMMANDS
        .iter()
        .find(|spec| spec.command[1..] == *name)
        .map(|spec| spec.action)
}

pub fn command_matches(query: &str) -> Vec<CommandSpec> {
    let query = query.to_lowercase();
    COMMANDS
        .iter()
        .filter(|spec| query.is_empty() || spec.command[1..].to_lowercase().contains(&query))
        .cloned()
        .collect()
}

pub fn command_search(query: &str) -> Vec<CommandSpec> {
    let query = query.trim().to_lowercase();
    COMMANDS
        .iter()
        .filter(|spec| {
            query.is_empty()
                || spec.command.to_lowercase().contains(&query)
                || spec.description.to_lowercase().contains(&query)
                || spec.category.to_lowercase().contains(&query)
        })
        .cloned()
        .collect()
}

pub fn skill_query(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("/skill ")?;
    if rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

pub fn skill_matches(skills: &[crate::skills::Skill], prefix: &str) -> Vec<String> {
    let prefix = prefix.to_lowercase();
    skills
        .iter()
        .filter(|skill| skill.name.to_lowercase().starts_with(&prefix))
        .map(|skill| skill.name.clone())
        .collect()
}

pub fn file_matches(query: &str, dir: &str) -> Vec<String> {
    let root = if dir.is_empty() { "." } else { dir };
    if let Some(slash) = query.rfind('/') {
        let (path, prefix) = query.split_at(slash + 1);
        let base = if path.is_empty() {
            root.to_string()
        } else if path == "/" {
            "/".to_string()
        } else {
            format!(
                "{}/{}",
                root.trim_end_matches('/'),
                path.trim_end_matches('/')
            )
        };
        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                if name.starts_with('.') && is_dir {
                    continue;
                }
                if name.to_lowercase().starts_with(&prefix.to_lowercase()) {
                    matches.push(format!("{path}{name}"));
                }
            }
        }
        matches.sort();
        return matches;
    }
    let mut preferred = Vec::new();
    let mut rest = Vec::new();
    let mut visited = 0;
    walk_files(
        root,
        "",
        &query.to_lowercase(),
        &mut preferred,
        &mut rest,
        &mut visited,
    );
    preferred.sort();
    rest.sort();
    preferred.extend(rest);
    preferred
}

fn walk_files(
    dir: &str,
    relative: &str,
    query: &str,
    preferred: &mut Vec<String>,
    rest: &mut Vec<String>,
    visited: &mut usize,
) {
    if *visited > 4000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        *visited += 1;
        if *visited > 4000 {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if name.starts_with('.') && is_dir {
            continue;
        }
        let path = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        if is_dir {
            if name == "target" || name == "node_modules" {
                continue;
            }
            walk_files(
                &format!("{}/{}", dir.trim_end_matches('/'), name),
                &path,
                query,
                preferred,
                rest,
                visited,
            );
        }
        let lower_path = path.to_lowercase();
        let lower_name = name.to_lowercase();
        if !query.is_empty()
            && !lower_name.starts_with(query)
            && !lower_path.starts_with(query)
            && !lower_path.contains(query)
        {
            continue;
        }
        let display = if is_dir { format!("{path}/") } else { path };
        if query.is_empty() || lower_name.starts_with(query) || lower_path.starts_with(query) {
            preferred.push(display);
        } else {
            rest.push(display);
        }
    }
}

#[derive(Clone)]
pub struct ToolLabel {
    pub text: String,
    pub keep_tail: bool,
}

fn label(text: String) -> ToolLabel {
    ToolLabel {
        text,
        keep_tail: false,
    }
}

fn generic_label(name: &str, running: bool) -> ToolLabel {
    label(if running {
        format!("Working: {name}")
    } else {
        name.to_string()
    })
}

fn line_range(offset: Option<usize>, limit: Option<usize>) -> String {
    let start = offset.unwrap_or(1);
    match limit {
        Some(limit) => format!(":{start}-{}", start + limit.saturating_sub(1)),
        None if offset.is_some() => format!(":{start}"),
        None => String::new(),
    }
}

pub fn tool_label(call: &ToolCall, running: bool) -> ToolLabel {
    #[derive(Default, serde::Deserialize)]
    #[serde(default)]
    struct Arguments {
        path: Option<String>,
        command: Option<String>,
        search: Option<String>,
        fetch: Option<String>,
        name: Option<String>,
        offset: Option<usize>,
        limit: Option<usize>,
    }
    let arguments: Arguments = serde_json::from_str(&call.arguments).unwrap_or_default();
    let path = arguments.path.as_deref().unwrap_or_default();
    let command = arguments.command.as_deref().unwrap_or_default();
    let name = arguments.name.as_deref().unwrap_or_default();
    let search = arguments.search.as_deref().unwrap_or_default();
    let fetch = arguments.fetch.as_deref().unwrap_or_default();
    let offset = arguments.offset;
    let limit = arguments.limit;
    match call.name.as_str() {
        "bash" => ToolLabel {
            text: format!(
                "{} {}",
                if running { "Running" } else { "Ran" },
                command.split_whitespace().collect::<Vec<_>>().join(" ")
            ),
            keep_tail: true,
        },
        "read" => label(format!(
            "{} {path}{}",
            if running { "Reading" } else { "Read" },
            line_range(offset, limit)
        )),
        "write" => label(format!(
            "{} {path}",
            if running { "Writing" } else { "Wrote" }
        )),
        "edit" => {
            let edits = crate::tools::edit_count(&call.arguments);
            let count = if edits > 1 {
                format!(" ({edits} edits)")
            } else {
                String::new()
            };
            label(format!(
                "{} {path}{count}",
                if running { "Editing" } else { "Edited" }
            ))
        }
        "web" if !search.is_empty() => label(format!(
            "{} \"{search}\"",
            if running { "Searching" } else { "Searched" }
        )),
        "web" if !fetch.is_empty() => label(format!(
            "{} {fetch}",
            if running { "Fetching" } else { "Fetched" }
        )),
        "load_skill" => label(format!(
            "{} {name}",
            if running { "Loading" } else { "Loaded" }
        )),
        _ => generic_label(call.name.as_str(), running),
    }
}

pub fn last_assistant_response(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant" && !message.content.is_empty())
        .map(|message| message.content.as_str())
}

pub fn rewind_items(messages: &[Message]) -> Vec<RewindItem> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(message_index, message)| {
            if message.role != "user" && message.role != "assistant" {
                return None;
            }
            if message.role == "user" && message.content.starts_with(session::COMPACTION_PREFIX) {
                return None;
            }
            let mut preview = message
                .content
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            if preview.is_empty() && message.role == "assistant" && !message.tool_calls.is_empty() {
                let count = message.tool_calls.len();
                preview = format!("({count} tool call{})", if count == 1 { "" } else { "s" });
            }
            if preview.is_empty() {
                return None;
            }
            Some(RewindItem {
                message_index,
                role: message.role.to_string(),
                preview,
            })
        })
        .collect()
}

pub fn filter_rewind_items(items: &[RewindItem], query: &str) -> Vec<RewindItem> {
    let query = query.trim().to_lowercase();
    items
        .iter()
        .filter(|item| {
            query.is_empty()
                || item.preview.to_lowercase().contains(&query)
                || item.role.to_lowercase().contains(&query)
        })
        .cloned()
        .collect()
}

pub fn rewind_entries(messages: &[Message], index: usize) -> Vec<Entry> {
    messages[..index.min(messages.len())]
        .iter()
        .cloned()
        .map(|message| Entry::Message { message })
        .collect()
}

pub fn load_session(store: &dyn Store, id: &str) -> Option<(String, Vec<Entry>)> {
    if id == "last" {
        let session_meta = store.list().into_iter().next()?;
        let entries = store.load(&session_meta.id)?;
        return Some((session_meta.id, entries));
    }
    store.load(id).map(|entries| (id.to_string(), entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn text(label: ToolLabel) -> String {
        label.text
    }

    #[test]
    fn labels_name_what_the_tool_works_on() {
        assert_eq!(
            text(tool_label(
                &call("web", r#"{"search":"rust async"}"#),
                false
            )),
            "Searched \"rust async\""
        );
        assert_eq!(
            text(tool_label(
                &call("web", r#"{"fetch":"https://example.com"}"#),
                true
            )),
            "Fetching https://example.com"
        );
        assert_eq!(
            text(tool_label(&call("load_skill", r#"{"name":"pdf"}"#), false)),
            "Loaded pdf"
        );
        assert_eq!(text(tool_label(&call("mystery", "{}"), false)), "mystery");
        assert_eq!(
            text(tool_label(&call("mystery", "{}"), true)),
            "Working: mystery"
        );
    }

    #[test]
    fn read_labels_show_the_requested_lines() {
        assert_eq!(
            text(tool_label(&call("read", r#"{"path":"a.rs"}"#), false)),
            "Read a.rs"
        );
        assert_eq!(
            text(tool_label(
                &call("read", r#"{"path":"a.rs","offset":10,"limit":5}"#),
                false
            )),
            "Read a.rs:10-14"
        );
        assert_eq!(
            text(tool_label(
                &call("read", r#"{"path":"a.rs","offset":10}"#),
                false
            )),
            "Read a.rs:10"
        );
        assert_eq!(
            text(tool_label(
                &call("read", r#"{"path":"a.rs","limit":5}"#),
                false
            )),
            "Read a.rs:1-5"
        );
    }

    #[test]
    fn edit_labels_count_batched_edits() {
        assert_eq!(
            text(tool_label(
                &call("edit", r#"{"path":"a.rs","oldText":"a","newText":"b"}"#),
                false
            )),
            "Edited a.rs"
        );
        assert_eq!(
            text(tool_label(
                &call(
                    "edit",
                    r#"{"path":"a.rs","edits":[{"oldText":"a","newText":"b"},{"oldText":"c","newText":"d"}]}"#
                ),
                false
            )),
            "Edited a.rs (2 edits)"
        );
    }

    #[test]
    fn only_bash_labels_keep_their_tail() {
        assert!(tool_label(&call("bash", r#"{"command":"ls"}"#), false).keep_tail);
        assert!(!tool_label(&call("read", r#"{"path":"a.rs"}"#), false).keep_tail);
    }

    #[test]
    fn parses_and_filters_commands() {
        assert_eq!(parse_command("/compact now"), Some(Command::Compact));
        assert_eq!(parse_command("missing"), None);
        assert_eq!(command_matches("res")[0].action, Command::Resume);
    }

    #[test]
    fn offers_skill_names_after_the_skill_command() {
        let skill = |name: &str, location: &str| crate::skills::Skill {
            name: name.into(),
            description: "does a thing".into(),
            location: location.into(),
        };
        let skills = vec![
            skill("code-review", "/tmp/review/SKILL.md"),
            skill("pdf-processing", "/tmp/pdf/SKILL.md"),
        ];
        assert_eq!(skill_query("/skill "), Some(""));
        assert_eq!(skill_query("/skill p"), Some("p"));
        assert_eq!(skill_query("/skill pdf done"), None);
        assert_eq!(skill_query("/skills"), None);
        assert_eq!(
            skill_matches(&skills, ""),
            ["code-review", "pdf-processing"]
        );
        assert_eq!(skill_matches(&skills, "p"), ["pdf-processing"]);
        assert_eq!(skill_matches(&skills, "zzz").len(), 0);
    }

    #[test]
    fn selects_last_assistant_response() {
        let messages = vec![
            Message {
                role: "assistant".into(),
                content: "first".into(),
                ..Message::default()
            },
            Message {
                role: "assistant".into(),
                content: "last".into(),
                ..Message::default()
            },
        ];
        assert_eq!(last_assistant_response(&messages), Some("last"));
    }

    #[test]
    fn builds_rewind_entries() {
        let messages = vec![Message::default(), Message::default()];
        assert_eq!(rewind_entries(&messages, 1).len(), 1);
    }
}
