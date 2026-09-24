//! Sentinel coverage: real secret files must be redacted, real code must not
//! be touched, and arbitrary input must never panic or leave the output
//! in a state a second pass changes.

use axe::sentinel::Sentinel;
use axe::{Message, ToolCall};

fn sentinel() -> Sentinel {
    let mut s = Sentinel::new();
    s.add_value("hunter2-secret-value");
    s
}

#[test]
fn redacts_common_secret_files() {
    let cases: &[(&str, &str)] = &[
        (
            "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "wJalrXUtnFEMI",
        ),
        (
            "DATABASE_URL=postgres://user:hunter2@host:5432/db",
            "hunter2",
        ),
        (
            "machine api.github.com login alice password ghp_abcdefghijklmnop",
            "ghp_abcdefghijklmnop",
        ),
        (
            r#"{"api_key": "sk-abcdefghijklmnopqrstuvwxyz"}"#,
            "sk-abcdef",
        ),
        ("Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
        ("password: hunter2", "hunter2"),
        ("run --token abcdefgh --verbose", "abcdefgh"),
        (
            "//registry.npmjs.org/:_authToken=npm_abcdefghijklmnop",
            "npm_abcdefghijklmnop",
        ),
        (
            "aws_access_key_id = AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
        ),
        (
            "token=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        ),
        (
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature1234567890",
            "eyJhbGciOiJIUzI1NiJ9",
        ),
        (r#"client_secret = "abc123def456ghi789jkl""#, "abc123def456"),
        ("SIGNING_KEY=Zm9vYmFyQmF6UXV4MTIz", "Zm9vYmFy"),
        ("known hunter2-secret-value here", "hunter2-secret-value"),
        (
            "-----BEGIN OPENSSH PRIVATE KEY-----\nMIIabc\n-----END OPENSSH PRIVATE KEY-----",
            "MIIabc",
        ),
    ];
    let s = sentinel();
    for (input, secret) in cases {
        let out = s.redact(input);
        assert!(
            !out.contains(secret),
            "secret leaked for input {input:?}: {out:?}"
        );
        assert!(
            out.contains("[REDACTED]"),
            "nothing redacted for input {input:?}: {out:?}"
        );
    }
}

#[test]
fn leaves_real_code_untouched() {
    let lines = [
        "let key = KeyCode::Enter;",
        "pub api_key: String,",
        "input_tokens: usize,",
        "output_tokens = 0;",
        "fn handle_key(&mut self, key: KeyEvent) -> bool {",
        "KeyModifiers::SHIFT | KeyModifiers::ALT",
        "let token_count = tokens.len();",
        "match c { KeyCode::Esc => break }",
        "const MIN_TOKEN_LEN: usize = 20;",
        "let token = bare_token(s, value_start);",
        r#"{"path": "/tmp/x", "content": "hello"}"#,
        "//! Outbound secret sentinel.",
        "let secret_sauce = 42;",
        "let password_hash = compute();",
        "keyboard = us;",
        "let api_key = api_key.into();",
        "self.api_key = String::new();",
        "Some(Response { message: assistant(\"done\"), usage })",
    ];
    let s = sentinel();
    for line in lines {
        let out = s.redact(line);
        assert_eq!(out, line, "code was mangled: {line:?} => {out:?}");
    }
}

#[test]
fn redact_message_leaves_what_the_agent_writes_alone() {
    let m = Message {
        role: "assistant".into(),
        content: "PASSWORD=hunter2-secret-value".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: r#"{"api_key":"hunter2-secret-value"}"#.into(),
        }],
        tool_call_id: String::new(),
        reasoning: "use hunter2-secret-value".into(),
        images: Vec::new(),
    };
    let r = sentinel().redact_message(&m);
    assert_eq!(r.content, m.content);
    assert_eq!(r.tool_calls[0].arguments, m.tool_calls[0].arguments);
    assert_eq!(r.reasoning, m.reasoning);
}

#[test]
fn redact_message_covers_what_the_user_writes_and_what_tools_return() {
    for role in ["user", "tool"] {
        let m = Message {
            role: role.into(),
            content: "my key is hunter2-secret-value".into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
            reasoning: String::new(),
            images: Vec::new(),
        };
        assert_eq!(
            sentinel().redact_message(&m).content,
            "my key is [REDACTED]"
        );
    }
}

#[test]
fn malformed_input_does_not_panic() {
    let inputs = [
        "-----BEGIN ",
        "-----BEGIN \n",
        "key=\"",
        "password 'unterminated",
        "a://b",
        "://",
        r#""key": "#,
        "-----BEGIN a -----END",
        "-----END b",
        "=\":",
        "\\\\",
        "password abá",
        "password abcá",
        "token abcá",
        "api_key 1á2b",
        "password a✓x",
        "passwd éx",
    ];
    let s = sentinel();
    for input in inputs {
        let once = s.redact(input);
        assert_eq!(s.redact(&once), once, "not idempotent: {input:?}");
    }
}

#[test]
fn multibyte_values_do_not_panic() {
    let s = sentinel();
    for input in [
        "password abá",
        "password abcá",
        "token abcá",
        "api_key 1á2b",
        "password a✓x",
        "passwd éx",
        "secret 12á3",
        "Authorization: ánonimo",
        "run --token abá --verbose",
    ] {
        let once = s.redact(input);
        assert_eq!(s.redact(&once), once, "not idempotent: {input:?}");
    }
}

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

#[test]
fn fuzz_never_panics_and_is_idempotent() {
    let s = sentinel();
    let alphabet: Vec<char> = "abcKEYsecret password=/:\"'`{[]}\\,@%0139"
        .chars()
        .chain("é🙈日本語\t\n\r-".chars())
        .collect();
    let mut state = 0x1234_5678_9abc_def0u64;
    for _ in 0..100_000 {
        let len = (lcg(&mut state) % 48) as usize;
        let mut input = String::new();
        for _ in 0..len {
            input.push(alphabet[(lcg(&mut state) as usize) % alphabet.len()]);
        }
        let once = s.redact(&input);
        assert_eq!(s.redact(&once), once, "not idempotent: {input:?}");
    }
}
