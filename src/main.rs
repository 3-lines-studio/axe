//! axe CLI: full-screen transcript TUI (fx-style) or one-shot prompt.

#![forbid(unsafe_code)]

use axe::run::{self, Outcome, RunOptions, Sink};
use axe::{Image, Message, OpenAI, Tool, ToolCall, Usage};
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
    images: Vec<Image>,
}

struct FileConfig {
    api_key: String,
    model: String,
    base: String,
    context_window: Option<usize>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [arg] if arg == "-V" || arg == "--version") {
        println!("axe {}", env!("CARGO_PKG_VERSION"));
        return;
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
    let mut prompt = prompt;
    if prompt.is_empty() && std::io::stdin().is_terminal() {
        if !cfg.images.is_empty() {
            eprintln!("error: --image needs a prompt; it cannot open the TUI");
            std::process::exit(2);
        }
        let tools = axe::tui::build_tools(&cfg.dir);
        let session_dir =
            axe::session::scope_dir(&axe_root(), std::path::Path::new(&work_dir(&cfg)));
        let tui_cfg = axe::tui::TuiConfig {
            base: cfg.base.clone(),
            model: cfg.model.clone(),
            system: resolve_system(&cfg, &tools),
            dir: cfg.dir.clone(),
            session_dir,
            api_key: api_key(&fc),
            resume: cfg.resume.clone(),
            context_window: fc.context_window,
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
    if cfg.resume.as_deref() == Some("") {
        eprintln!("error: --resume needs last or a session id in one-shot mode");
        std::process::exit(2);
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
        images: Vec::new(),
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
        if a == "--" {
            rest = args[i + 1..].to_vec();
            break;
        }
        let Some(stripped) = a.strip_prefix('-').filter(|s| !s.is_empty()) else {
            rest = args[i..].to_vec();
            break;
        };
        let stripped = stripped.strip_prefix('-').unwrap_or(stripped);
        let (name, inline) = match stripped.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (stripped.to_string(), None),
        };
        match name.as_str() {
            "base" | "model" | "system" | "C" | "i" | "image" => {
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
                    "i" | "image" => cfg.images.push(axe::image::attach(&v)?),
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
         \x20 -V, --version  show the axe version\n\
         \x20 -base URL    OpenAI-compatible API base URL (default \"https://api.openai.com/v1\")\n\
         \x20 -model NAME  model name (default \"gpt-4.1-mini\")\n\
         \x20 -system TEXT  system prompt (default: built-in)\n\
         \x20 -C DIR       working directory for tools\n\
         \x20 -i, --image SOURCE  attach an image to the prompt: a file path or an http(s) URL; repeatable\n\
         \x20 -r, --resume  open the session picker\n\
         \x20 --resume last  resume the most recent session\n\
         \x20 --resume ID   resume a saved session by id\n\
         \x20 --           end of flags: every later argument is part of the prompt\n\
         \n\
         With no prompt and a TTY, starts the interactive transcript TUI\n\
         (fresh session; \"/resume\" reopens saved ones).\n\
         With no prompt and no TTY, reads the prompt from stdin."
    );
}

struct CliSink {
    input: usize,
    output: usize,
    threshold: Option<usize>,
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
        self.threshold
            .is_some_and(|threshold| input.saturating_add(output) > threshold)
    }
}

fn persist_oneshot(
    dir: &str,
    resume_id: Option<&str>,
    entries: &[axe::session::Entry],
) -> std::io::Result<()> {
    let Some(id) = resume_id else {
        return Ok(());
    };
    axe::session::continue_archived(dir, id, entries).map(|_| ())
}

fn fail_oneshot(
    dir: &str,
    resume_id: Option<&str>,
    entries: &[axe::session::Entry],
    msg: String,
) -> ! {
    eprintln!("{msg}");
    if let Err(e) = persist_oneshot(dir, resume_id, entries) {
        eprintln!("error: save session: {e}");
    }
    std::process::exit(1);
}

fn one_shot(cfg: &Config, fc: &FileConfig, prompt: &[String]) {
    let start = Instant::now();
    let session_dir = axe::session::scope_dir(&axe_root(), std::path::Path::new(&work_dir(cfg)));
    let mut resume_id = None;
    let mut history = Vec::new();
    let mut session_entries = Vec::new();
    if let Some(id) = &cfg.resume {
        axe::session::archive_live(&session_dir);
        let loaded = if id == "last" {
            axe::session::list_sessions(&session_dir)
                .into_iter()
                .next()
                .map(|s| (s.id, axe::session::load_session(&s.path)))
        } else {
            axe::session::load_by_id(&session_dir, id).map(|entries| (id.clone(), entries))
        };
        match loaded {
            Some((id, entries)) => {
                resume_id = Some(id);
                history = axe::session::context_messages(&entries);
                axe::session::drop_incomplete_tool_calls(&mut history);
                session_entries = entries;
            }
            None => {
                eprintln!("error: no such session: {id}");
                std::process::exit(1);
            }
        }
    }
    history.push(Message {
        role: "user".into(),
        content: prompt.join(" "),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
        reasoning: String::new(),
        images: cfg.images.clone(),
    });
    session_entries.push(axe::session::Entry::Message {
        message: history.last().unwrap().clone(),
    });
    let tools = axe::tui::build_tools(&cfg.dir);
    let system = resolve_system(cfg, &tools);
    let provider = OpenAI::new(cfg.base.clone(), api_key(fc));
    let mut sink = CliSink {
        input: 0,
        output: 0,
        threshold: fc.context_window.map(|window| window.saturating_sub(16384)),
    };
    let opts = RunOptions {
        model: &cfg.model,
        system: &system,
        tools: &tools,
        max_turns: usize::MAX,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let mut overflow_retried = false;
    let msgs;
    loop {
        let end = run::run_stream(&provider, &opts, &history, &cancel, &mut sink);
        session_entries.extend(
            end.messages[history.len()..]
                .iter()
                .cloned()
                .map(|message| axe::session::Entry::Message { message }),
        );
        if end.usage.input > 0 || end.usage.output > 0 {
            session_entries.push(axe::session::Entry::Usage {
                input: end.usage.input,
                output: end.usage.output,
                cached_input: end.context.cached_input,
                context_input: end.context.input,
                context_output: end.context.output,
            });
        }
        match end.outcome {
            Outcome::Failed(error) => {
                if !overflow_retried && axe::session::is_overflow_error(&error) {
                    overflow_retried = true;
                    eprintln!("error: {error}; compacting");
                    history = compact_or_fail(
                        &provider,
                        &cfg.model,
                        &session_dir,
                        resume_id.as_deref(),
                        &mut session_entries,
                    );
                    continue;
                }
                fail_oneshot(
                    &session_dir,
                    resume_id.as_deref(),
                    &session_entries,
                    format!("error: {error}"),
                );
            }
            Outcome::Compact => {
                history = compact_or_fail(
                    &provider,
                    &cfg.model,
                    &session_dir,
                    resume_id.as_deref(),
                    &mut session_entries,
                );
                continue;
            }
            Outcome::Cancelled => {
                fail_oneshot(
                    &session_dir,
                    resume_id.as_deref(),
                    &session_entries,
                    "error: interrupted".into(),
                );
            }
            Outcome::MaxTurns => {
                fail_oneshot(
                    &session_dir,
                    resume_id.as_deref(),
                    &session_entries,
                    "error: stopped: max turns reached".into(),
                );
            }
            Outcome::Done => {
                msgs = end.messages;
                break;
            }
        }
    }
    if let Err(e) = persist_oneshot(&session_dir, resume_id.as_deref(), &session_entries) {
        eprintln!("error: save session: {e}");
        std::process::exit(1);
    }
    if sink.input + sink.output > 0 {
        eprintln!(
            "tokens: {} in / {} out · {}",
            tok(sink.input),
            tok(sink.output),
            fmt_dur(start.elapsed())
        );
    }
    for m in &msgs {
        if m.role == "assistant" && !m.content.is_empty() && m.tool_calls.is_empty() {
            println!("{}", m.content);
        }
    }
}

fn compact_or_fail(
    provider: &OpenAI,
    model: &str,
    dir: &str,
    resume_id: Option<&str>,
    entries: &mut Vec<axe::session::Entry>,
) -> Vec<Message> {
    match axe::session::compact(provider, model, entries) {
        Ok((summary, tokens_before, retained)) => {
            entries.push(axe::session::Entry::Compaction {
                summary,
                tokens_before,
                timestamp: axe::session::now_ms(),
                retained,
            });
            let mut out = axe::session::context_messages(entries);
            axe::session::drop_incomplete_tool_calls(&mut out);
            out
        }
        Err(e) => fail_oneshot(
            dir,
            resume_id,
            entries,
            format!("error: compaction failed: {e}"),
        ),
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
    if !cfg.system.is_empty() {
        return cfg.system.clone();
    }
    let mut out = axe::system_prompt(tools);
    out.push_str(&format!("\nCurrent working directory: {}", work_dir(cfg)));
    if let Some(user) = axe::user_system_prompt() {
        out.push_str("\n\n");
        out.push_str(&user);
    }
    out
}

fn axe_root() -> String {
    match axe::config_dir() {
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
    };
    let Some(dir) = axe::config_dir() else {
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
    use axe::new_tool;

    fn tool(name: &'static str, snippet: &'static str) -> Tool {
        let mut tool = new_tool(name, "", "{}", |_: ()| String::new());
        tool.snippet = snippet;
        tool
    }

    fn empty_config() -> FileConfig {
        FileConfig {
            api_key: String::new(),
            model: String::new(),
            base: String::new(),
            context_window: None,
        }
    }

    #[test]
    fn parse_args_collects_images() {
        let fc = empty_config();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (cfg, _) = parse_args(
            &args(&[
                "-i",
                "https://example.com/a.png",
                "--image=https://example.com/b.png",
            ]),
            &fc,
        )
        .unwrap();
        assert_eq!(cfg.images.len(), 2);
        assert_eq!(cfg.images[0].url, "https://example.com/a.png");
        assert_eq!(cfg.images[1].url, "https://example.com/b.png");
    }

    #[test]
    fn parse_args_end_of_flags() {
        let fc = empty_config();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // `--` stops flag parsing: a prompt that begins with a dash is passed
        // through, and the marker itself is consumed.
        let (cfg, rest) = parse_args(&args(&["--", "-fix the bug"]), &fc).unwrap();
        assert_eq!(rest, ["-fix the bug"]);
        assert!(cfg.dir.is_empty());

        // Flags before `--` still apply.
        let (cfg, rest) = parse_args(&args(&["-model", "m", "--", "-x"]), &fc).unwrap();
        assert_eq!(cfg.model, "m");
        assert_eq!(rest, ["-x"]);

        // A lone `-` is an operand, not a flag.
        let (_, rest) = parse_args(&args(&["-"]), &fc).unwrap();
        assert_eq!(rest, ["-"]);

        // `--` with nothing after it yields no prompt.
        let (_, rest) = parse_args(&args(&["--"]), &fc).unwrap();
        assert!(rest.is_empty());

        // `--help` after `--` is a prompt, not the help flag.
        let (_, rest) = parse_args(&args(&["--", "--help"]), &fc).unwrap();
        assert_eq!(rest, ["--help"]);

        // Unknown flags are still rejected.
        assert!(parse_args(&args(&["-nope"]), &fc).is_err());
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

    #[test]
    fn default_system_prompt_is_the_tool_list() {
        let tools = vec![tool("bash", "run a command"), tool("edit", "")];
        let cfg = Config {
            base: String::new(),
            model: String::new(),
            system: String::new(),
            dir: "/tmp/work".into(),
            resume: None,
            images: Vec::new(),
        };
        let head =
            "Available tools:\n- bash: run a command\n\nCurrent working directory: /tmp/work";
        assert!(resolve_system(&cfg, &tools).starts_with(head));
    }
}
