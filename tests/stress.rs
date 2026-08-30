//! Deterministic fuzz/stress harness for the axe library surface.
//!
//! Seeds come from the real ~/.config/axe data (config, SYSTEM.md, user
//! commands, archived session transcripts) when present, plus built-in
//! seeds. Every mutation is reproducible from a fixed seed.
//!
//! Run: cargo test --release --test stress

use axe::markdown::{Markdown, ansi};
use axe::openai::StreamEvent;
use axe::{Message, OpenAI, Request, new_tool};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::panic;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;

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

    fn bytes(&mut self, out: &mut [u8]) {
        for b in out {
            *b = self.next() as u8;
        }
    }
}

fn corpus() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    let ax_root = Path::new(&config_home).join("axe");
    if let Ok(text) = std::fs::read(ax_root.join("config")) {
        seeds.push(text);
    }
    if let Ok(text) = std::fs::read(ax_root.join("SYSTEM.md")) {
        seeds.push(text);
    }
    if let Ok(entries) = std::fs::read_dir(ax_root.join("commands")) {
        for e in entries.flatten() {
            if let Ok(text) = std::fs::read(e.path()) {
                seeds.push(text);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(ax_root.join("sessions")) {
        for e in entries.flatten() {
            if let Ok(text) = std::fs::read(e.path()) {
                seeds.push(text);
            }
        }
    }
    seeds.extend([
        b"".to_vec(),
        b"#".to_vec(),
        b"```\n".to_vec(),
        b"| a | b |\n|---|---|\n| 1 | 2 |".to_vec(),
        b"*em* **strong** `code` [link](https://x.test)".to_vec(),
        b"\xff\xfe\x00\x80\xc3\x28\xe2\x82\x28\xf0\x28\x8c\x28".to_vec(),
        b"---\ndescription: hi\n---\nbody".to_vec(),
        b"  - [x] task\n  - [ ] todo".to_vec(),
        b"[^1] footnote\n\n[^1]: def".to_vec(),
        b"# h1\n## h2\n### h3\n#### h4\n##### h5\n###### h6".to_vec(),
        b"> quote\n> nested > quote".to_vec(),
        b"http://example.com/path?q=1&r=2 (parens)".to_vec(),
        b"<https://auto.link> <user@example.com>".to_vec(),
        b"![alt](img.png)".to_vec(),
        b"line 1  \nline 2\\\nline 3".to_vec(),
        b"\\*escaped\\* \\# \\` \\[ \\] \\_".to_vec(),
        b"___".to_vec(),
        b"***".to_vec(),
        b"---".to_vec(),
        b"1. one\n2. two\n3. three".to_vec(),
        b"term\n: definition".to_vec(),
        "\u{2026}\u{2014}\u{00e9}\u{4e2d}\u{6587}\u{1f600}"
            .as_bytes()
            .to_vec(),
        b"\x1b[31mred\x1b[0m plain".to_vec(),
        b"foo_bar_baz_qux".to_vec(),
        "a".repeat(2000).into_bytes(),
        b"```rust\nfn main() {}\n```".to_vec(),
        b"    indented code\n    more".to_vec(),
        "a".repeat(300).into_bytes(),
    ]);
    seeds.extend([
        format!("{}\ntext", "> ".repeat(200)).into_bytes(),
        format!("| c |\n|---|\n{}", "| x |\n".repeat(50)).into_bytes(),
        "|".repeat(40000).into_bytes(),
        format!("```\n{}", "x\n".repeat(1000)).into_bytes(),
        "[".repeat(10000).into_bytes(),
        "_".repeat(10000).into_bytes(),
        "*".repeat(10000).into_bytes(),
        format!("[^l{}]: body", "o".repeat(5000)).into_bytes(),
        "a\r\nb\r\nc".as_bytes().to_vec(),
        "\x00\x01\x02\x03\x1b\x7f".as_bytes().to_vec(),
        format!("{}\n{}", "    ".repeat(5000), "code").into_bytes(),
        format!("#{}\n", "#".repeat(500)).into_bytes(),
        "<a>".repeat(5000).into_bytes(),
        "(x)".repeat(5000).into_bytes(),
        "`".repeat(5000).into_bytes(),
        format!("{}end", " ".repeat(30000)).into_bytes(),
        "\u{1f600}".repeat(2000).as_bytes().to_vec(),
    ]);
    seeds
}

fn guard<T>(name: &str, seed: u64, f: impl FnOnce() -> T) -> Option<T> {
    match panic::catch_unwind(panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(_) => {
            eprintln!("PANIC in {name} seed={seed}");
            None
        }
    }
}

fn assert_sane(name: &str, seed: u64, out: &str) {
    let n = out.len();
    if n > 4 * 1024 * 1024 {
        eprintln!("BLOWUP {name} seed={seed} output={n} bytes");
        panic!("{name}: output grew to {n} bytes");
    }
}

fn fuzz_markdown(rng: &mut Rng, seed: u64, input: &[u8]) {
    let s = String::from_utf8_lossy(input);
    guard("markdown render", seed, || {
        assert_sane("markdown render", seed, &Markdown::render(&s));
    });
    guard("markdown stream", seed, || {
        let mut md = Markdown::new();
        let n = s.len();
        let mut i = 0;
        while i < n {
            let step = 1 + rng.below(64);
            let end = (i + step).min(n);
            md.push(&s[i..end]);
            i = end;
        }
        let out = md.finish();
        assert_sane("markdown stream", seed, &out);
    });
    guard("markdown on_block", seed, || {
        let mut md = Markdown::new();
        let blocks = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let count = std::rc::Rc::clone(&blocks);
        md.set_on_block(move |_b: axe::markdown::Block, _out: &mut String| {
            count.set(count.get() + 1);
        });
        md.push(&s);
        let out = md.finish();
        assert_sane("markdown on_block", seed, &out);
        assert!(blocks.get() <= 4096, "too many blocks {}", blocks.get());
    });
}

const TOKENS: [&str; 20] = [
    "```", "``", "`", "**", "__", "*", "_", "[", "]", "(", ")", "|", "^", "<", ">", "\n\n", "\n",
    "#", "-", "\\",
];

fn mutate(rng: &mut Rng, buf: &mut Vec<u8>, rounds: usize) {
    for _ in 0..rounds {
        match rng.below(5) {
            0 => {
                let i = rng.below(buf.len() + 1);
                let b = rng.next() as u8;
                if i < buf.len() {
                    buf[i] = b;
                } else {
                    buf.push(b);
                }
            }
            1 => {
                let i = rng.below(buf.len() + 1);
                let n = rng.below(16);
                let mut junk = vec![0u8; n];
                rng.bytes(&mut junk);
                buf.splice(i..i, junk);
            }
            2 => {
                if !buf.is_empty() {
                    let i = rng.below(buf.len());
                    let n = rng.below(buf.len() - i + 1).min(32);
                    buf.drain(i..i + n);
                }
            }
            3 => {
                if !buf.is_empty() {
                    let i = rng.below(buf.len());
                    buf.truncate(i);
                }
            }
            _ => {
                let i = rng.below(buf.len() + 1);
                let tok = TOKENS[rng.below(TOKENS.len())];
                buf.splice(i..i, tok.as_bytes().iter().copied());
            }
        }
    }
}

fn fuzz_ansi(rng: &mut Rng, seed: u64, input: &[u8]) {
    guard("ansi writers", seed, || {
        let mut out = String::new();
        ansi::write_dim(&mut out, input);
        ansi::write_horizontal_rule(&mut out);
        assert_sane("ansi writers", seed, &out);
    });
    guard("highlight", seed, || {
        for label in ["rust", "python", "js", "go", "json", "markdown", "none"] {
            if let Some(p) = axe::markdown::highlight::resolve(label) {
                let out = axe::markdown::highlight::highlight(input, p);
                assert_sane("highlight", seed, &out);
            }
        }
    });
    let _ = rng;
}

fn fuzz_session(_rng: &mut Rng, seed: u64, input: &[u8]) {
    let dir =
        std::env::temp_dir().join(format!("axe-stress-session-{seed}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("session.jsonl"), input);
    guard("session load_live", seed, || {
        let msgs = axe::session::load_live(dir.to_str().unwrap());
        assert!(msgs.len() <= input.iter().filter(|&&b| b == b'\n').count() + 1);
    });
    guard("session list", seed, || {
        let _ = axe::session::list_sessions(dir.to_str().unwrap());
    });
    guard("session archive", seed, || {
        let _ = axe::session::archive_live(dir.to_str().unwrap());
    });
    guard("session save roundtrip", seed, || {
        let msgs = axe::session::load_live(dir.to_str().unwrap());
        axe::session::save_live(dir.to_str().unwrap(), &msgs);
        let again = axe::session::load_live(dir.to_str().unwrap());
        assert_eq!(msgs, again);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn fuzz_commands(_rng: &mut Rng, seed: u64, input: &[u8]) {
    let dir = std::env::temp_dir().join(format!("axe-stress-cmd-{seed}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(dir.join("commands"));
    let _ = std::fs::write(dir.join("commands").join("fuzz.md"), input);
    guard("commands load+expand", seed, || {
        let cmds = axe::tui::load_user_commands(dir.to_str().unwrap());
        if let Some(uc) = cmds.into_iter().find(|c| c.name == "fuzz") {
            for rest in ["", "arg one", "a".repeat(64).as_str()] {
                let out = axe::tui::expand_user_command(&uc, rest);
                assert_sane("commands expand", seed, &out);
            }
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn fuzz_tools(_rng: &mut Rng, seed: u64, input: &[u8]) {
    let dir = std::env::temp_dir().join(format!("axe-stress-tools-{seed}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let target = dir.join("f.txt");
    let _ = std::fs::write(&target, input);
    let path = target.to_str().unwrap().to_string();

    let read = axe::tools::read();
    let write = axe::tools::write();
    let edit = axe::tools::edit();
    let bash = axe::tools::bash(dir.to_str().unwrap());

    guard("tool read", seed, || {
        let args = serde_json::json!({"path": path, "offset": 1, "limit": 2}).to_string();
        let out = (read.run)(&args, &mut |_| {});
        assert_sane("tool read", seed, &out);
    });
    guard("tool write", seed, || {
        let content = String::from_utf8_lossy(input);
        let args = serde_json::json!({"path": path, "content": content}).to_string();
        let out = (write.run)(&args, &mut |_| {});
        assert_sane("tool write", seed, &out);
    });
    guard("tool edit", seed, || {
        let needle = String::from_utf8_lossy(&input[..input.len().min(32)]);
        let args =
            serde_json::json!({"path": path, "edits": [{"oldText": needle, "newText": "X"}]})
                .to_string();
        let out = (edit.run)(&args, &mut |_| {});
        assert_sane("tool edit", seed, &out);
    });
    guard("tool bash safe", seed, || {
        for cmd in ["echo hi", "true", "pwd", "printf 'x\n'"] {
            let args = serde_json::json!({"command": cmd}).to_string();
            let out = (bash.run)(&args, &mut |_| {});
            assert_sane("tool bash", seed, &out);
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn fuzz_new_tool(_rng: &mut Rng, seed: u64, input: &[u8]) {
    #[derive(serde::Deserialize)]
    #[expect(dead_code)]
    struct Args {
        a: Option<String>,
        b: Option<u64>,
        c: Option<Vec<String>>,
    }
    let tool = new_tool("fuzz", "d", "{}", |_: Args| String::new());
    let s = String::from_utf8_lossy(input);
    guard("new_tool args", seed, || {
        let _ = (tool.run)(&s, &mut |_| {});
    });
}

fn fuzz_tui_text(rng: &mut Rng, seed: u64, input: &[u8]) {
    let s = String::from_utf8_lossy(input);
    guard("tui wrap_ansi", seed, || {
        let width = 1 + rng.below(40);
        let rows = axe::tui::wrap_ansi(&s, width);
        assert!(rows.len() <= s.chars().count().max(1) + 1);
        for row in rows {
            assert_sane("tui wrap row", seed, &row);
        }
    });
}

fn fuzz_sse(_rng: &mut Rng, seed: u64, payloads: &[Vec<u8>]) {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let n = payloads.len();
    let owned: Vec<Vec<u8>> = payloads.to_vec();
    let handle = thread::spawn(move || {
        for body in &owned {
            let (mut sock, _) = match server.accept() {
                Ok(x) => x,
                Err(_) => return,
            };
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let r = sock.read(&mut buf).unwrap_or(0);
                if r == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..r]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: ",
            );
            let _ = sock.write_all(body.len().to_string().as_bytes());
            let _ = sock.write_all(b"\r\nConnection: close\r\n\r\n");
            let _ = sock.write_all(body);
        }
    });

    let p = OpenAI::new(format!("http://{addr}"), "k");
    let req = Request {
        model: "m",
        system: "",
        messages: &[Message {
            role: "user".into(),
            content: "go".into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
        }],
        tools: &[],
    };
    for i in 0..n {
        guard(&format!("sse {i}"), seed.wrapping_add(i as u64), || {
            let (tx, rx) = mpsc::channel();
            let resp = p
                .complete_stream(&req, &Arc::new(AtomicBool::new(false)), tx)
                .join();
            assert!(resp.is_ok(), "stream thread panicked");
            if let Ok(Ok(r)) = resp {
                assert_sane("sse content", seed, &r.message.content);
                for c in &r.message.tool_calls {
                    assert_sane("sse call name", seed, &c.name);
                    assert_sane("sse call args", seed, &c.arguments);
                }
            }
            for ev in rx.iter() {
                match ev {
                    StreamEvent::Content(c) => assert_sane("sse event content", seed, &c),
                    StreamEvent::ToolCall(c) => assert_sane("sse toolcall", seed, &c.arguments),
                    _ => {}
                }
            }
        });
    }
    let _ = handle.join();
}

fn sse_payload(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mode = rng.below(6);
    let s = String::from_utf8_lossy(input);
    match mode {
        0 => format!("data: {s}\n\n").into_bytes(),
        1 => {
            let idx = rng.below(8);
            let json = serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{"index": idx, "id": "c", "function": {"name": "bash", "arguments": "{}"}}]}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            });
            format!("data: {json}\n\n").into_bytes()
        }
        2 => {
            let json = serde_json::json!({
                "choices": [{"delta": {"content": s}}],
                "usage": {"prompt_tokens": rng.below(1000), "completion_tokens": rng.below(1000)}
            });
            format!("data: {json}\n\n").into_bytes()
        }
        3 => {
            let mut out = Vec::new();
            for _ in 0..3 {
                let json = serde_json::json!({
                    "choices": [{"delta": {"content": rng.below(2).to_string()}}]
                });
                out.extend_from_slice(format!("data: {json}\n\n").as_bytes());
            }
            out
        }
        4 => b"data: [DONE]\n\n".to_vec(),
        _ => input.to_vec(),
    }
}

#[test]
fn stress_markdown_and_ansi() {
    let seeds = corpus();
    let mut cases = 0;
    for (si, base) in seeds.iter().enumerate() {
        let mut rng = Rng(si as u64 ^ 0x9E3779B97F4A7C15);
        for it in 0..3000 {
            let seed = (si as u64) << 20 | it;
            let mut buf = base.clone();
            mutate(&mut rng, &mut buf, 24);
            fuzz_markdown(&mut rng, seed, &buf);
            fuzz_ansi(&mut rng, seed, &buf);
            cases += 1;
        }
    }
    eprintln!("stress_markdown_and_ansi: {cases} cases");
}

#[test]
#[ignore]
fn stress_text_and_session() {
    let seeds = corpus();
    let mut cases = 0;
    for (si, base) in seeds.iter().enumerate() {
        let mut rng = Rng(si as u64 ^ 0xD1B54A32D192ED03);
        for it in 0..500 {
            let seed = (si as u64) << 20 | it;
            let mut buf = base.clone();
            mutate(&mut rng, &mut buf, 16);
            fuzz_session(&mut rng, seed, &buf);
            fuzz_commands(&mut rng, seed, &buf);
            fuzz_tools(&mut rng, seed, &buf);
            fuzz_new_tool(&mut rng, seed, &buf);
            fuzz_tui_text(&mut rng, seed, &buf);
            cases += 1;
        }
    }
    eprintln!("stress_text_and_session: {cases} cases");
}

#[test]
fn stress_load_config_via_binary() {
    let bin = env!("CARGO_BIN_EXE_axe");
    let dir = std::env::temp_dir().join(format!("axe-cfg-bin-{}", std::process::id()));
    let cfg_dir = dir.join("axe");
    std::fs::create_dir_all(&cfg_dir).unwrap();

    let mut rng = Rng(0xFEEDFACE);
    let keys = ["api_key", "model", "base", "other", ""];
    let mut cases = 0;
    for _ in 0..150 {
        let mut text = String::new();
        for _ in 0..rng.below(12) {
            let k = keys[rng.below(keys.len())];
            let v: String = (0..rng.below(40))
                .map(|_| {
                    let b = rng.next() as u8;
                    if b == b'\n' || b == 0 { 'x' } else { b as char }
                })
                .collect();
            text.push_str(&format!("{k}={v}\n"));
        }
        std::fs::write(cfg_dir.join("config"), &text).unwrap();
        let out = std::process::Command::new(bin)
            .arg("-h")
            .env("XDG_CONFIG_HOME", &dir)
            .env_remove("OPENAI_API_KEY")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "binary crashed on fuzzed config:\n{text}"
        );
        cases += 1;
    }

    if let Some(home) = std::env::var_os("HOME") {
        let real = std::path::Path::new(&home).join(".config").join("axe");
        if real.join("config").exists() {
            let _ = std::fs::create_dir_all(cfg_dir.clone());
            let _ = std::fs::copy(real.join("config"), cfg_dir.join("config"));
            let _ = std::fs::copy(real.join("SYSTEM.md"), cfg_dir.join("SYSTEM.md"));
            let out = std::process::Command::new(bin)
                .arg("-h")
                .env("XDG_CONFIG_HOME", &dir)
                .env_remove("OPENAI_API_KEY")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "binary crashed on real ~/.config/axe/config"
            );
        }
    }

    std::fs::remove_dir_all(&dir).ok();
    eprintln!("stress_load_config_via_binary: {cases} fuzzed + real config");
}

#[test]
fn sse_huge_index_capped() {
    let payload = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":2000000000,\"id\":\"c\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec();
    fuzz_sse(&mut Rng(7), 7, &[payload]);
}

#[test]
fn session_load_by_id_sanitizes_paths() {
    let dir = std::env::temp_dir().join(format!("axe-loadid-{}", std::process::id()));
    let store = dir.join("sessions");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("123.jsonl"),
        "{\"Role\":\"user\",\"Content\":\"hi\"}\n",
    )
    .unwrap();
    std::fs::write(dir.join("secret.jsonl"), "not a session").unwrap();
    let d = dir.to_str().unwrap();

    let ok = axe::session::load_by_id(d, "123").expect("valid id loads");
    let msgs = axe::session::context_messages(&ok);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "hi");

    for bad in ["", ".", "..", "../secret", "..\\secret", "/etc/passwd"] {
        assert!(
            axe::session::load_by_id(d, bad).is_none(),
            "id {bad:?} must be rejected"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stress_sse_streaming() {
    let seeds = corpus();
    let mut payloads = Vec::new();
    for (si, base) in seeds.iter().enumerate() {
        let mut rng = Rng(si as u64 ^ 0x94D049BB133111EB);
        for _ in 0..40 {
            let mut buf = base.clone();
            mutate(&mut rng, &mut buf, 8);
            payloads.push(sse_payload(&mut rng, &buf));
        }
    }
    for _ in 0..8 {
        payloads.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n".to_vec());
        payloads.push(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n".to_vec(),
        );
        payloads.push(b"data: garbage\n\n".to_vec());
        payloads.push(b"data: [DONE]\n\ndata: extra\n\n".to_vec());
    }
    fuzz_sse(&mut Rng(42), 42, &payloads);
    eprintln!("stress_sse_streaming: {} payloads", payloads.len());
}

#[test]
fn edit_multi_edit_and_line_endings() {
    let dir = std::env::temp_dir().join(format!("axe-edit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.txt");
    let edit = axe::tools::edit();
    let p = path.to_str().unwrap();

    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let args = serde_json::json!({"path": p, "edits": [
        {"oldText": "one", "newText": "ONE"},
        {"oldText": "three", "newText": "THREE"},
    ]})
    .to_string();
    let out = (edit.run)(&args, &mut |_| {});
    assert!(out.starts_with("Successfully replaced 2 block(s)"), "{out}");
    assert!(out.contains("Diff:"), "{out}");
    assert!(out.contains("-1 one"), "{out}");
    assert!(out.contains("+1 ONE"), "{out}");
    assert!(out.contains("-3 three"), "{out}");
    assert!(out.contains("+3 THREE"), "{out}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\nTHREE\n");

    std::fs::write(&path, "a\r\nb\r\n").unwrap();
    let args =
        serde_json::json!({"path": p, "edits": [{"oldText": "b", "newText": "c"}]}).to_string();
    let out = (edit.run)(&args, &mut |_| {});
    assert!(out.starts_with("Successfully"), "{out}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nc\r\n");

    std::fs::write(&path, "x\nx\n").unwrap();
    let args =
        serde_json::json!({"path": p, "edits": [{"oldText": "x", "newText": "y"}]}).to_string();
    let out = (edit.run)(&args, &mut |_| {});
    assert!(out.contains("occurrences"), "{out}");

    std::fs::write(&path, "abcdef\n").unwrap();
    let args = serde_json::json!({"path": p, "edits": [
        {"oldText": "abc", "newText": "X"},
        {"oldText": "bcd", "newText": "Y"},
    ]})
    .to_string();
    let out = (edit.run)(&args, &mut |_| {});
    assert!(out.contains("overlap"), "{out}");

    std::fs::write(&path, "abc\n").unwrap();
    let args =
        serde_json::json!({"path": p, "edits": [{"oldText": "abc", "newText": "abc"}]}).to_string();
    let out = (edit.run)(&args, &mut |_| {});
    assert!(out.contains("No changes"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_offset_paging() {
    let dir = std::env::temp_dir().join(format!("axe-read-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.txt");
    std::fs::write(&path, "line1\nline2\nline3\nline4\n").unwrap();
    let read = axe::tools::read();
    let p = path.to_str().unwrap();

    let args = serde_json::json!({"path": p, "offset": 2, "limit": 2}).to_string();
    let out = (read.run)(&args, &mut |_| {});
    assert!(out.starts_with("line2\nline3"), "{out}");
    // The file has exactly 4 lines, so one remains after reading 2-3.
    assert!(out.contains("1 more lines"), "{out}");
    assert!(out.contains("offset=4"), "{out}");

    let args = serde_json::json!({"path": p, "offset": 10}).to_string();
    let out = (read.run)(&args, &mut |_| {});
    assert!(out.contains("beyond end of file"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn write_creates_parent_dirs() {
    let dir = std::env::temp_dir().join(format!("axe-write-{}", std::process::id()));
    let path = dir.join("sub").join("f.txt");
    let write = axe::tools::write();
    let args = serde_json::json!({"path": path.to_str().unwrap(), "content": "hi"}).to_string();
    let out = (write.run)(&args, &mut |_| {});
    assert!(out.starts_with("wrote"), "{out}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn bash_timeout_kills() {
    let bash = axe::tools::bash("");
    let args = serde_json::json!({"command": "sleep 5", "timeout": 1}).to_string();
    let start = std::time::Instant::now();
    let out = (bash.run)(&args, &mut |_| {});
    let elapsed = start.elapsed();
    assert!(out.contains("timed out"), "{out}");
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
}

#[test]
fn bash_rejects_zero_timeout() {
    let bash = axe::tools::bash("");
    let args = serde_json::json!({"command": "echo hi", "timeout": 0}).to_string();
    let out = (bash.run)(&args, &mut |_| {});
    assert!(out.contains("invalid timeout"), "{out}");
}

#[test]
fn bash_captures_output_and_status() {
    let bash = axe::tools::bash("");
    let args = serde_json::json!({"command": "echo hello"}).to_string();
    assert_eq!((bash.run)(&args, &mut |_| {}), "hello\n");
    let args = serde_json::json!({"command": "exit 3"}).to_string();
    let out = (bash.run)(&args, &mut |_| {});
    assert_eq!(out, "error: exit status 3");
    let args = serde_json::json!({"command": "printf 'o' ; printf 'e' >&2"}).to_string();
    let out = (bash.run)(&args, &mut |_| {});
    assert_eq!(out, "oe");
}

#[test]
fn session_old_format_and_compaction_roundtrip() {
    let dir = std::env::temp_dir().join(format!("axe-session-entries-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();

    std::fs::write(
        dir.join("session.jsonl"),
        "{\"Role\":\"user\",\"Content\":\"hi\"}\n",
    )
    .unwrap();
    let entries = axe::session::load_live(d);
    assert_eq!(entries.len(), 1);
    let msgs = axe::session::context_messages(&entries);
    assert_eq!(msgs[0].content, "hi");

    let entry = axe::session::Entry::Compaction {
        summary: "done stuff".into(),
        tokens_before: 100,
        timestamp: 1,
        retained: vec![Message {
            role: "assistant".into(),
            content: "recent".into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
        }],
    };
    let usage = axe::session::Entry::Usage {
        input: 120,
        output: 8,
        cached_input: 80,
        context_input: 100,
        context_output: 8,
    };
    axe::session::save_live(d, &[entry, usage]);
    let entries = axe::session::load_live(d);
    assert!(matches!(
        entries[1],
        axe::session::Entry::Usage {
            input: 120,
            output: 8,
            cached_input: 80,
            context_input: 100,
            context_output: 8
        }
    ));
    let msgs = axe::session::context_messages(&entries);
    assert_eq!(msgs.len(), 2);
    assert!(msgs[0].content.contains("done stuff"));
    assert!(msgs[0].content.contains("<summary>"));
    assert_eq!(msgs[1].content, "recent");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn session_compaction_is_append_only() {
    let dir = std::env::temp_dir().join(format!("axe-compact-append-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();

    let msg = |role: &str, content: &str| Message {
        role: role.into(),
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    };
    let mut entries = vec![
        axe::session::Entry::Message {
            message: msg("user", "findable old detail ZEBRA"),
        },
        axe::session::Entry::Message {
            message: msg("assistant", "noted"),
        },
    ];
    axe::session::save_live(d, &entries);

    // Compaction appends; nothing is rewritten or dropped from disk.
    entries.push(axe::session::Entry::Compaction {
        summary: "earlier work summarized".into(),
        tokens_before: 100,
        timestamp: 1,
        retained: vec![msg("assistant", "recent")],
    });
    entries.push(axe::session::Entry::Message {
        message: msg("user", "new question"),
    });
    axe::session::save_live(d, &entries);
    assert_eq!(axe::session::load_live(d).len(), 4, "history stays on disk");

    // The projection supersedes everything before the summary.
    let msgs = axe::session::context_messages(&axe::session::load_live(d));
    assert_eq!(msgs.len(), 3);
    assert!(msgs[0].content.contains("earlier work summarized"));
    assert_eq!(msgs[1].content, "recent");
    assert_eq!(msgs[2].content, "new question");

    // Pre-compaction history remains searchable.
    let hits = axe::session::search(d, "ZEBRA");
    assert_eq!(hits.len(), 1, "old history must stay searchable");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn session_overflow_patterns() {
    assert!(axe::session::is_overflow_error(
        "openai: 400: This model's maximum context length is 131072 tokens"
    ));
    assert!(axe::session::is_overflow_error(
        "anthropic: prompt is too long: 213462 tokens > 200000 maximum"
    ));
    assert!(!axe::session::is_overflow_error(
        "openai: 429: rate limit reached"
    ));
    assert!(!axe::session::is_overflow_error(
        "openai: 529: Throttling error"
    ));
}

#[test]
fn session_search_finds_text() {
    let dir = std::env::temp_dir().join(format!("axe-search-{}", std::process::id()));
    let store = dir.join("sessions");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("123.jsonl"),
        "{\"Role\":\"user\",\"Content\":\"fix the overflow bug\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("session.jsonl"),
        "{\"Role\":\"user\",\"Content\":\"unrelated thing\"}\n",
    )
    .unwrap();
    let d = dir.to_str().unwrap();
    let hits = axe::session::search(d, "overflow");
    assert_eq!(hits.len(), 1, "hits: {hits:?}");
    assert!(hits[0].text.contains("overflow"));
    assert!(axe::session::search(d, "zzz").is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn prompt_template_substitution() {
    use axe::tui::{UserCommand, expand_user_command, parse_command_args, substitute_args};
    let uc = |content: &str| UserCommand {
        name: "x".into(),
        description: String::new(),
        content: content.into(),
    };
    assert_eq!(parse_command_args("hi there"), vec!["hi", "there"]);
    assert_eq!(
        parse_command_args("say \"hi there\" now"),
        vec!["say", "hi there", "now"]
    );
    assert_eq!(parse_command_args("a 'b c'"), vec!["a", "b c"]);

    let args = parse_command_args("one two three");
    assert_eq!(
        substitute_args("first=$1 last=$3", &args),
        "first=one last=three"
    );
    assert_eq!(substitute_args("all=$@", &args), "all=one two three");
    assert_eq!(
        substitute_args("all=$ARGUMENTS", &args),
        "all=one two three"
    );
    assert_eq!(substitute_args("d=${2:-default}", &args), "d=two");
    assert_eq!(substitute_args("d=${9:-default}", &args), "d=default");
    assert_eq!(substitute_args("from=${@:2}", &args), "from=two three");
    assert_eq!(substitute_args("from=${@:2:1}", &args), "from=two");
    assert_eq!(substitute_args("d=${1:-fallback}", &[]), "d=fallback");

    assert_eq!(expand_user_command(&uc("say $1"), "hi"), "say hi");
    assert_eq!(
        expand_user_command(&uc("say $ARGUMENTS"), "hi there"),
        "say hi there"
    );
    assert_eq!(expand_user_command(&uc("plain"), "hi"), "plain\n\nhi");
}

#[test]
fn builtin_tool_snippets_present() {
    let tools = axe::tui::build_tools("");
    for name in ["read", "write", "edit", "bash"] {
        let t = tools.iter().find(|t| t.name == name).expect(name);
        assert!(!t.snippet.is_empty(), "{name} has no snippet");
    }
}
