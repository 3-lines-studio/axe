//! axe CLI: full-screen transcript TUI (fx-style) or one-shot prompt.

#![forbid(unsafe_code)]

use axe::run::{self, Outcome, RunOptions, Sink};
use axe::{Agent, Error, Event, Message, OpenAI, Tool, ToolCall};
use std::cell::RefCell;
use std::io::{IsTerminal, Read};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

struct Config {
    base: String,
    model: String,
    system: String,
    dir: String,
    resume: Option<String>,
    session: Option<String>,
    events: bool,
    messages: Option<String>,
    list_models: bool,
    compact: Option<String>,
}

struct FileConfig {
    api_key: String,
    model: String,
    base: String,
    context_window: Option<usize>,
    compaction_threshold: Option<usize>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(pos) = args.iter().position(|a| a == "--search") {
        let text = args.get(pos + 1).map(String::as_str).unwrap_or("");
        if text.is_empty() {
            eprintln!("usage: axe --search <text>");
            std::process::exit(1);
        }
        let session_dir =
            axe::session::scope_dir(&ax_root(), &std::env::current_dir().unwrap_or_default());
        for h in axe::session::search(&session_dir, text) {
            let id = if h.id == "live" {
                "live".to_string()
            } else {
                h.id.clone()
            };
            if h.title.is_empty() {
                println!("({id}) — {}", h.text);
            } else {
                println!("{} ({id}) — {}", h.title, h.text);
            }
        }
        std::process::exit(0);
    }
    let fc = load_config();
    let (cfg, prompt) = match parse_args(&args, &fc) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}");
            usage();
            std::process::exit(2);
        }
    };
    if !cfg.dir.is_empty()
        && let Err(e) = std::env::set_current_dir(&cfg.dir)
    {
        eprintln!("error: change directory: {e}");
        std::process::exit(1);
    }
    if cfg.messages.is_some() && !cfg.events {
        eprintln!("error: --messages requires --events");
        std::process::exit(2);
    }
    if cfg.list_models {
        list_models(&cfg, &fc);
        return;
    }
    if let Some(path) = cfg.compact.as_deref() {
        compact_messages(&cfg, &fc, path);
        return;
    }
    let mut prompt = prompt;
    if prompt.is_empty()
        && std::io::stdin().is_terminal()
        && !(cfg.events && cfg.messages.is_some())
    {
        let tools = axe::tui::build_tools(&cfg.dir);
        let session_dir =
            axe::session::scope_dir(&ax_root(), std::path::Path::new(&work_dir(&cfg)));
        let tui_cfg = axe::tui::TuiConfig {
            base: cfg.base.clone(),
            model: cfg.model.clone(),
            system: resolve_system(&cfg, &tools),
            dir: cfg.dir.clone(),
            ax_root: ax_root(),
            session_dir,
            api_key: api_key(&fc),
            resume: cfg.resume.clone(),
            context_window: fc.context_window,
            compaction_threshold: fc.compaction_threshold,
        };
        if let Err(e) = axe::tui::run(tui_cfg) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cfg.events && cfg.messages.is_some() {
        one_shot(&cfg, &fc, &prompt);
        return;
    }
    if prompt.is_empty() {
        let mut b = String::new();
        if std::io::stdin().read_to_string(&mut b).is_err() {
            eprintln!("error: read stdin");
            std::process::exit(1);
        }
        prompt = vec![b];
    }
    one_shot(&cfg, &fc, &prompt);
}

fn parse_args(args: &[String], fc: &FileConfig) -> Result<(Config, Vec<String>), String> {
    let mut cfg = Config {
        base: if fc.base.is_empty() {
            "https://api.openai.com/v1".into()
        } else {
            fc.base.clone()
        },
        model: if fc.model.is_empty() {
            "gpt-4.1-mini".into()
        } else {
            fc.model.clone()
        },
        system: String::new(),
        dir: String::new(),
        resume: None,
        session: None,
        events: false,
        messages: None,
        list_models: false,
        compact: None,
    };
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-h" || a == "--help" {
            usage();
            std::process::exit(0);
        }
        if a == "--list-models" {
            cfg.list_models = true;
            i += 1;
            continue;
        }
        if a == "--events" {
            cfg.events = true;
            i += 1;
            continue;
        }
        if a == "-r" {
            cfg.resume = Some(String::new());
            i += 1;
            continue;
        }
        if a == "--resume" || a == "resume" {
            // Bare resume opens the picker; a following non-flag names the target.
            if let Some(next) = args.get(i + 1)
                && !next.starts_with('-')
            {
                cfg.resume = Some(next.clone());
                i += 2;
                continue;
            }
            cfg.resume = Some(String::new());
            i += 1;
            continue;
        }
        if let Some(v) = a.strip_prefix("--resume=") {
            cfg.resume = Some(v.to_string());
            i += 1;
            continue;
        }
        let Some(stripped) = a.strip_prefix('-') else {
            rest = args[i..].to_vec();
            break;
        };
        let stripped = stripped.strip_prefix('-').unwrap_or(stripped);
        let (name, inline) = match stripped.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (stripped.to_string(), None),
        };
        match name.as_str() {
            "base" | "model" | "system" | "C" | "session" | "messages" | "compact" => {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or_else(|| format!("flag needs an argument: {a}"))?
                    }
                };
                match name.as_str() {
                    "base" => cfg.base = v,
                    "model" => cfg.model = v,
                    "system" => cfg.system = v,
                    "C" => cfg.dir = v,
                    "session" => cfg.session = Some(v),
                    "messages" => cfg.messages = Some(v),
                    "compact" => cfg.compact = Some(v),
                    _ => unreachable!(),
                }
            }
            _ => return Err(format!("flag provided but not defined: {a}")),
        }
        i += 1;
    }
    Ok((cfg, rest))
}

fn usage() {
    eprintln!(
        "Usage: axe [flags] [prompt]\n\
         \n\
         Flags:\n\
         \x20 -base URL    OpenAI-compatible API base URL (default \"https://api.openai.com/v1\")\n\
         \x20 -model NAME  model name (default \"gpt-4.1-mini\")\n\
         \x20 -system TEXT  system prompt (default: built-in)\n\
         \x20 -C DIR       working directory for tools\n\
         \x20 --session FILE  use an explicit session file\n\
         \x20 --events      emit JSONL events on stdout\n\
         \x20 --messages FILE  use a JSON message array with --events\n\
         \x20 --list-models  print model names as JSON\n\
         \x20 --compact FILE  compact a JSONL session and print JSON\n\
         \x20 -r, --resume  open the session picker\n\
         \x20 --resume last  resume the most recent session\n\
         \x20 --resume ID   resume a saved session by id\n\
         \n\
         With no prompt and a TTY, starts the interactive transcript TUI\n\
         (fresh session; \"/resume\" reopens saved ones).\n\
         With no prompt and no TTY, reads the prompt from stdin.
         A prompt of the form /name [args] expands a user command from
         ~/.config/axe/commands/<name>.md (same expansion as the TUI)."
    );
}

fn list_models(cfg: &Config, fc: &FileConfig) {
    let provider = OpenAI::new(cfg.base.clone(), api_key(fc));
    match provider.list_models() {
        Ok(models) => println!("{}", serde_json::to_string(&models).unwrap()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn compact_messages(cfg: &Config, fc: &FileConfig, path: &str) {
    let entries = match axe::session::load_path(std::path::Path::new(path)) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let provider = OpenAI::new(cfg.base.clone(), api_key(fc));
    match axe::session::compact(&provider, &cfg.model, &entries) {
        Ok((summary, tokens_before, retained)) => println!(
            "{}",
            serde_json::json!({
                "summary": summary,
                "tokens_before": tokens_before,
                "retained": retained
            })
        ),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

struct EventSink {
    input: std::sync::mpsc::Receiver<String>,
    compaction_threshold: Option<usize>,
}

impl EventSink {
    fn emit(&self, value: serde_json::Value) {
        use std::io::Write;
        println!("{value}");
        let _ = std::io::stdout().flush();
    }
}

impl Sink for EventSink {
    fn assistant_delta(&mut self, text: &str) {
        self.emit(serde_json::json!({"type": "assistant_delta", "text": text}));
    }

    fn assistant_done(&mut self) {
        self.emit(serde_json::json!({"type": "assistant_done"}));
    }

    fn assistant(&mut self, _turn: usize, message: &Message, _usage: axe::Usage) {
        self.emit(serde_json::json!({"type": "message", "message": message}));
    }

    fn tool_start(&mut self, call: &ToolCall) {
        self.emit(serde_json::json!({
            "type": "tool_start",
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments
        }));
    }

    fn tool_delta(&mut self, call: &ToolCall, text: &str) {
        self.emit(serde_json::json!({"type": "tool_delta", "id": call.id, "text": text}));
    }

    fn tool_result(&mut self, call: &ToolCall) {
        self.emit(serde_json::json!({"type": "tool_done", "id": call.id}));
    }

    fn tokens(&mut self, input: usize, output: usize, cached_input: usize) {
        self.emit(serde_json::json!({
            "type": "usage",
            "input": input,
            "output": output,
            "cached_input": cached_input
        }));
    }

    fn tool(&mut self, _turn: usize, message: &Message) {
        self.emit(serde_json::json!({
            "type": "tool_result",
            "id": message.tool_call_id,
            "output": message.content
        }));
        self.emit(serde_json::json!({"type": "message", "message": message}));
    }

    fn should_compact(&mut self, input: usize, output: usize) -> bool {
        self.compaction_threshold
            .is_some_and(|threshold| input.saturating_add(output) > threshold)
    }

    fn pending_user_input(&mut self) -> Option<String> {
        self.input.try_recv().ok()
    }
}

fn one_shot_events(cfg: &Config, fc: &FileConfig, prompt: &[String]) {
    let (history, old_len) = if let Some(path) = cfg.messages.as_deref() {
        let data = std::fs::read(path).unwrap_or_else(|e| event_failure(&e.to_string()));
        let messages = serde_json::from_slice::<Vec<Message>>(&data)
            .unwrap_or_else(|e| event_failure(&e.to_string()));
        let len = messages.len();
        (messages, len)
    } else {
        let mut messages = match cfg.session.as_deref() {
            Some(path) => match axe::session::load_path(std::path::Path::new(path)) {
                Ok(entries) => axe::session::context_messages(&entries),
                Err(e) => event_failure(&e),
            },
            None => Vec::new(),
        };
        messages.push(Message {
            role: "user".into(),
            content: expand_user_command(&prompt.join(" "), &ax_root()),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
        });
        let old_len = messages.len().saturating_sub(1);
        (messages, old_len)
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    if !std::io::stdin().is_terminal() {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match value.get("type").and_then(|value| value.as_str()) {
                    Some("cancel") => cancel.store(true, Ordering::Relaxed),
                    Some("steer") => {
                        if let Some(text) = value.get("text").and_then(|value| value.as_str()) {
                            let _ = tx.send(text.to_string());
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    let tools = axe::tui::build_tools(&cfg.dir);
    let system = resolve_system(cfg, &tools);
    let provider = OpenAI::new(cfg.base.clone(), api_key(fc));
    let mut sink = EventSink {
        input: rx,
        compaction_threshold: fc
            .compaction_threshold
            .or_else(|| fc.context_window.map(|window| window.saturating_sub(16384))),
    };
    let end = run::run_stream(
        &provider,
        &RunOptions {
            model: &cfg.model,
            system: &system,
            tools: &tools,
            max_turns: usize::MAX,
        },
        &history,
        &cancel,
        &mut sink,
    );
    if let Some(path) = cfg.session.as_deref()
        && let Err(e) =
            axe::session::append_messages(std::path::Path::new(path), &end.messages[old_len..])
    {
        event_failure(&e);
    }
    println!(
        "{}",
        serde_json::json!({"type": "result", "messages": end.messages, "usage": {
            "input": end.usage.input,
            "output": end.usage.output,
            "cached_input": end.usage.cached_input
        }})
    );
    let (outcome, failed) = match end.outcome {
        Outcome::Done => ("done", false),
        Outcome::Cancelled => ("cancelled", false),
        Outcome::Compact => ("compact", false),
        Outcome::MaxTurns => ("max_turns", true),
        Outcome::Failed(message) => {
            println!(
                "{}",
                serde_json::json!({"type": "error", "message": message})
            );
            ("failed", true)
        }
    };
    println!(
        "{}",
        serde_json::json!({"type": "done", "outcome": outcome})
    );
    if failed {
        std::process::exit(1);
    }
}

fn event_failure(message: &str) -> ! {
    println!(
        "{}",
        serde_json::json!({"type": "error", "message": message})
    );
    println!(
        "{}",
        serde_json::json!({"type": "done", "outcome": "failed"})
    );
    std::process::exit(1);
}

fn one_shot(cfg: &Config, fc: &FileConfig, prompt: &[String]) {
    if cfg.events {
        one_shot_events(cfg, fc, prompt);
        return;
    }
    let stats = Rc::new(RefCell::new((0usize, 0usize)));
    let s2 = stats.clone();
    let on = move |e: Event| {
        let mut st = s2.borrow_mut();
        st.0 += e.usage.input;
        st.1 += e.usage.output;
        drop(st);
        let m = &e.message;
        if m.role == "assistant" {
            if !m.content.is_empty() && !m.tool_calls.is_empty() {
                eprintln!("{}", m.content);
            }
            for c in &m.tool_calls {
                eprintln!("[{}] {} {}", e.turn, c.name, render_args(c));
            }
        } else if m.role == "tool" {
            eprintln!("[{}] -> {}", e.turn, render_result(&m.content));
        }
    };
    let start = Instant::now();
    let mut agent = build_agent(cfg, fc, on);
    let mut history = match cfg.session.as_deref() {
        Some(path) => match axe::session::load_path(std::path::Path::new(path)) {
            Ok(entries) => axe::session::context_messages(&entries),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        None => Vec::new(),
    };
    history.push(Message {
        role: "user".into(),
        content: expand_user_command(&prompt.join(" "), &ax_root()),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    });
    let old_len = history.len().saturating_sub(1);
    let msgs = match agent.run(&history) {
        Ok(msgs) => msgs,
        Err(Error::MaxTurns(h)) => {
            eprintln!("stopped: axe: max turns reached");
            h
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    if let Some(path) = cfg.session.as_deref()
        && let Err(e) = axe::session::append_messages(std::path::Path::new(path), &msgs[old_len..])
    {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    let (i, o) = *stats.borrow();
    if i + o > 0 {
        eprintln!(
            "tokens: {} in / {} out · {}",
            tok(i),
            tok(o),
            fmt_dur(start.elapsed())
        );
    }
    let pretty = std::io::stdout().is_terminal();
    for m in &msgs[old_len..] {
        if m.role == "assistant" && !m.content.is_empty() && m.tool_calls.is_empty() {
            if pretty {
                let rendered = axe::markdown::Markdown::render(&m.content);
                print!("{rendered}");
            } else {
                println!("{}", m.content);
            }
        }
    }
}

fn expand_user_command(prompt: &str, ax_root: &str) -> String {
    let Some(cmd) = prompt.strip_prefix('/') else {
        return prompt.to_string();
    };
    let (name, rest) = match cmd.split_once(' ') {
        Some((n, r)) => (n, r.trim()),
        None => (cmd, ""),
    };
    let Some(uc) = axe::tui::load_user_commands(ax_root)
        .into_iter()
        .find(|c| c.name == name)
    else {
        return prompt.to_string();
    };
    axe::tui::expand_user_command(&uc, rest)
}

fn build_agent(cfg: &Config, fc: &FileConfig, on: impl FnMut(Event) + 'static) -> Agent<OpenAI> {
    let tools = axe::tui::build_tools(&cfg.dir);
    let system = resolve_system(cfg, &tools);
    Agent::new(OpenAI::new(cfg.base.clone(), api_key(fc)))
        .model(cfg.model.clone())
        .system(system)
        .tools(tools)
        .on(on)
}

fn api_key(fc: &FileConfig) -> String {
    if let Ok(k) = std::env::var("OPENAI_API_KEY")
        && !k.is_empty()
    {
        return k;
    }
    fc.api_key.clone()
}

fn work_dir(cfg: &Config) -> String {
    if !cfg.dir.is_empty() {
        return cfg.dir.clone();
    }
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn resolve_system(cfg: &Config, tools: &[Tool]) -> String {
    if cfg.system.is_empty() {
        system_prompt(tools, &work_dir(cfg))
    } else {
        cfg.system.clone()
    }
}

fn system_prompt(tools: &[Tool], dir: &str) -> String {
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

fn config_dir() -> Option<std::path::PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
        && !x.is_empty()
    {
        return Some(std::path::PathBuf::from(x));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".config"))
}

fn ax_root() -> String {
    match config_dir() {
        Some(d) => d.join("axe").display().to_string(),
        None => work_dir_abs(),
    }
}

fn work_dir_abs() -> String {
    std::env::current_dir()
        .map(|p| p.join(".axe").display().to_string())
        .unwrap_or_else(|_| ".axe".to_string())
}

fn load_config() -> FileConfig {
    let mut c = FileConfig {
        api_key: String::new(),
        model: String::new(),
        base: String::new(),
        context_window: None,
        compaction_threshold: None,
    };
    let Some(dir) = config_dir() else {
        return c;
    };
    let Ok(text) = std::fs::read_to_string(dir.join("axe").join("config")) else {
        return c;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let mut val = v.trim().to_string();
        if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
            val = val[1..val.len() - 1].to_string();
        }
        match k.trim() {
            "api_key" => c.api_key = val,
            "model" => c.model = val,
            "base" => c.base = val,
            "context_window" => c.context_window = val.parse().ok(),
            "compaction_threshold" => c.compaction_threshold = val.parse().ok(),
            _ => {}
        }
    }
    c
}

fn render_args(call: &ToolCall) -> String {
    #[derive(serde::Deserialize)]
    struct A {
        path: Option<String>,
        content: Option<String>,
        command: Option<String>,
    }
    if let Ok(a) = serde_json::from_str::<A>(&call.arguments) {
        match call.name.as_str() {
            "bash" => {
                if let Some(c) = a.command
                    && !c.is_empty()
                {
                    return c;
                }
            }
            "read" | "edit" => {
                if let Some(p) = a.path
                    && !p.is_empty()
                {
                    return p;
                }
            }
            "write" => {
                if let Some(p) = a.path
                    && !p.is_empty()
                {
                    return format!(
                        "{} ({} bytes)",
                        p,
                        a.content.as_deref().map(|c| c.len()).unwrap_or(0)
                    );
                }
            }
            _ => {}
        }
    }
    call.arguments.trim().to_string()
}

fn render_result(s: &str) -> String {
    if s.contains("error:") {
        s.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_default()
    } else {
        first_line(s)
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn tok(n: usize) -> String {
    if n < 1000 {
        return n.to_string();
    }
    format!("{:.1}k", n as f64 / 1000.0)
}

fn fmt_dur(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = secs.round() as u64;
    format!("{}m{}s", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_user_command_cases() {
        let dir = std::env::temp_dir().join(format!("axe-uc-{}", std::process::id()));
        let cmds = dir.join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(
            cmds.join("commit.md"),
            "---\ndescription: stage and commit\n---\n\nstage everything now",
        )
        .unwrap();
        std::fs::write(cmds.join("args.md"), "say: $ARGUMENTS").unwrap();
        let root = dir.to_str().unwrap();

        assert_eq!(expand_user_command("/commit", root), "stage everything now");
        assert_eq!(
            expand_user_command("/commit blablabla", root),
            "stage everything now\n\nblablabla"
        );
        assert_eq!(expand_user_command("/args hi there", root), "say: hi there");
        assert_eq!(expand_user_command("/missing x", root), "/missing x");
        assert_eq!(expand_user_command("not a command", root), "not a command");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_args_cases() {
        let cases: Vec<(&str, &str, &str)> = vec![
            ("bash", r#"{"command":"go test ./..."}"#, "go test ./..."),
            ("read", r#"{"path":"main.go"}"#, "main.go"),
            ("edit", r#"{"path":"a.go","old":"x","new":"y"}"#, "a.go"),
            (
                "write",
                r#"{"path":"b.txt","content":"hello"}"#,
                "b.txt (5 bytes)",
            ),
            ("custom", r#"{"q":1}"#, r#"{"q":1}"#),
            ("bash", "not json", "not json"),
        ];
        for (name, args, want) in cases {
            let call = ToolCall {
                id: String::new(),
                name: name.into(),
                arguments: args.into(),
            };
            assert_eq!(render_args(&call), want, "case {name}");
        }
    }

    #[test]
    fn tok_cases() {
        assert_eq!(tok(999), "999");
        assert_eq!(tok(1234), "1.2k");
    }

    #[test]
    fn render_result_cases() {
        let fail = "# pkg/a\n./a.go:12:3: undefined: Foo\nerror: exit status 1\n";
        assert_eq!(render_result(fail), "error: exit status 1");
        assert_eq!(render_result("ok axe 0.003s\nmore"), "ok axe 0.003s");
        assert_eq!(render_result("build error: bad\nnext"), "next");
    }

    #[test]
    fn fmt_dur_cases() {
        assert_eq!(fmt_dur(std::time::Duration::from_millis(1500)), "1.5s");
        assert_eq!(fmt_dur(std::time::Duration::from_secs(65)), "1m5s");
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    #[test]
    fn parse_args_fuzz() {
        let fc = FileConfig {
            api_key: String::new(),
            model: String::new(),
            base: String::new(),
            context_window: None,
            compaction_threshold: None,
        };
        let tokens = [
            "-base",
            "-model",
            "-system",
            "-C",
            "-r",
            "--resume",
            "--resume=",
            "-",
            "--",
            "foo",
            "",
            "=x",
            "-x",
            "--x",
            "-base=",
            "-model=x",
            "prompt",
            "with space",
        ];
        for si in 0..64u64 {
            let mut rng = Rng(si ^ 0xABCDEF1234567890);
            for _ in 0..200 {
                let n = rng.below(6);
                let mut args = Vec::new();
                for _ in 0..n {
                    args.push(tokens[rng.below(tokens.len())].to_string());
                }
                if args.iter().any(|a| a == "-h" || a == "--help") {
                    continue;
                }
                let _ = parse_args(&args, &fc);
            }
        }
    }

    #[test]
    fn load_config_real_file_roundtrip() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let path = std::path::Path::new(&home)
            .join(".config")
            .join("axe")
            .join("config");
        if !path.exists() {
            return;
        }
        let c = load_config();
        let text = std::fs::read_to_string(&path).unwrap();
        let expect = |k: &str| -> String {
            text.lines()
                .find_map(|l| {
                    let (kk, v) = l.split_once('=')?;
                    (kk.trim() == k).then(|| v.trim().trim_matches('"').to_string())
                })
                .unwrap_or_default()
        };
        assert_eq!(c.base, expect("base"), "base round-trip");
        assert_eq!(c.model, expect("model"), "model round-trip");
        assert_eq!(c.api_key, expect("api_key"), "api_key round-trip");
    }
}
