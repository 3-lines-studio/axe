//! axe CLI: full-screen transcript TUI (fx-style) or one-shot prompt.

#![forbid(unsafe_code)]

use axe::run::{self, Outcome, RunOptions, Sink};
use axe::{Message, OpenAI, Tool, ToolCall, Usage};
use std::io::{IsTerminal, Read};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

struct Config {
    base: String,
    model: String,
    system: String,
    dir: String,
    resume: Option<String>,
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
    let mut prompt = prompt;
    if prompt.is_empty() && std::io::stdin().is_terminal() {
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
    };
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-h" || a == "--help" {
            usage();
            std::process::exit(0);
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
            "base" | "model" | "system" | "C" => {
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
         \x20 -r, --resume  open the session picker\n\
         \x20 --resume last  resume the most recent session\n\
         \x20 --resume ID   resume a saved session by id\n\
         \n\
         With no prompt and a TTY, starts the interactive transcript TUI\n\
         (fresh session; \"/resume\" reopens saved ones).\n\
         With no prompt and no TTY, reads the prompt from stdin."
    );
}

struct CliSink {
    input: usize,
    output: usize,
    compaction_threshold: Option<usize>,
}

impl Sink for CliSink {
    fn assistant(&mut self, turn: usize, message: &Message, usage: Usage) {
        self.input += usage.input;
        self.output += usage.output;
        if !message.content.is_empty() && !message.tool_calls.is_empty() {
            eprintln!("{}", message.content);
        }
        for call in &message.tool_calls {
            eprintln!("[{turn}] {} {}", call.name, render_args(call));
        }
    }

    fn tool(&mut self, turn: usize, message: &Message) {
        eprintln!("[{turn}] -> {}", render_result(&message.content));
    }

    fn should_compact(&mut self, input: usize, output: usize) -> bool {
        self.compaction_threshold
            .is_some_and(|threshold| input.saturating_add(output) > threshold)
    }
}

fn one_shot(cfg: &Config, fc: &FileConfig, prompt: &[String]) {
    let start = Instant::now();
    let history = vec![Message {
        role: "user".into(),
        content: prompt.join(" "),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    }];
    let tools = axe::tui::build_tools(&cfg.dir);
    let system = resolve_system(cfg, &tools);
    let provider = OpenAI::new(cfg.base.clone(), api_key(fc));
    let mut sink = CliSink {
        input: 0,
        output: 0,
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
        &Arc::new(AtomicBool::new(false)),
        &mut sink,
    );
    if let Outcome::Failed(error) = &end.outcome {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
    let msgs = end.messages;
    if sink.input + sink.output > 0 {
        eprintln!(
            "tokens: {} in / {} out · {}",
            tok(sink.input),
            tok(sink.output),
            fmt_dur(start.elapsed())
        );
    }
    let pretty = std::io::stdout().is_terminal();
    for m in &msgs {
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
