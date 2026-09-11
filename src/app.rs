use crate::session::{self, Entry};
use crate::{Message, ToolCall};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    New,
    Resume,
    Rewind,
    Compact,
    Copy,
    Image,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub input: usize,
    pub output: usize,
    pub context_input: usize,
    pub cached_input: usize,
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

pub fn tool_label(call: &ToolCall, running: bool) -> String {
    #[derive(serde::Deserialize)]
    struct Arguments {
        path: Option<String>,
        command: Option<String>,
    }
    let arguments: Option<Arguments> = serde_json::from_str(&call.arguments).ok();
    let path = arguments
        .as_ref()
        .and_then(|arguments| arguments.path.as_deref())
        .unwrap_or_default();
    let command = arguments
        .as_ref()
        .and_then(|arguments| arguments.command.as_deref())
        .unwrap_or_default();
    match call.name.as_str() {
        "bash" => {
            let command = command.split_whitespace().collect::<Vec<_>>().join(" ");
            let truncated = command.chars().count() > 120;
            let mut command: String = command.chars().take(120).collect();
            if truncated {
                command.push('…');
            }
            if running {
                format!("Running {command}")
            } else {
                format!("Ran {command}")
            }
        }
        "read" => format!("{} {path}", if running { "Reading" } else { "Read" }),
        "write" => format!("{} {path}", if running { "Writing" } else { "Wrote" }),
        "edit" => format!("{} {path}", if running { "Editing" } else { "Edited" }),
        _ => format!("Working: {}", call.name),
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
                role: message.role.clone(),
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

pub fn session_usage(entries: &[Entry]) -> SessionUsage {
    let mut usage = SessionUsage::default();
    for entry in entries {
        let Entry::Usage {
            input,
            output,
            cached_input,
            context_input,
            ..
        } = entry
        else {
            continue;
        };
        usage.input += input;
        usage.output += output;
        usage.context_input = *context_input;
        usage.cached_input = *cached_input;
    }
    usage
}

pub fn load_session(dir: &str, id: &str) -> Option<(String, Vec<Entry>)> {
    if id == "last" {
        return session::list_sessions(dir)
            .into_iter()
            .next()
            .map(|session_meta| {
                let entries = session::load_session(&session_meta.path);
                (session_meta.id, entries)
            });
    }
    session::load_by_id(dir, id).map(|entries| (id.to_string(), entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_filters_commands() {
        assert_eq!(parse_command("/compact now"), Some(Command::Compact));
        assert_eq!(parse_command("missing"), None);
        assert_eq!(command_matches("res")[0].action, Command::Resume);
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
