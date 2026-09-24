//! Outbound secret sentinel.
//!
//! Every request body the agent sends to the model passes through
//! [`Sentinel::redact`], so a secret that reached the transcript (a `cat
//! .env`, a pasted key, a tool error) never reaches the provider. Layers:
//! known literal values, PEM blocks, credentials in URLs, provider token
//! shapes, and sensitive `KEY=VALUE` / `"key": value` / `--flag value` /
//! `key value` assignments. Replacement is exact and idempotent; the local
//! transcript keeps the real value.

use crate::Message;
use std::sync::OnceLock;

const REDACTED: &str = "[REDACTED]";
const MIN_KNOWN_LEN: usize = 4;
const MIN_TOKEN_LEN: usize = 20;
const MIN_JWT_LEN: usize = 40;

#[derive(Default)]
pub struct Sentinel {
    values: Vec<String>,
}

impl Sentinel {
    pub fn new() -> Self {
        Self::default()
    }
    /// Literal values to replace verbatim, longest first.
    pub fn add_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if value.len() < MIN_KNOWN_LEN || self.values.iter().any(|v| v == &value) {
            return;
        }
        self.values.push(value);
        self.values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    }

    /// Harvest the process environment: a sensitive-named variable whose
    /// value looks like a secret (long enough, not a path) is worth matching
    /// verbatim even when it has no recognizable prefix.
    pub fn from_env() -> Self {
        let mut sentinel = Self::new();
        for (key, value) in std::env::vars() {
            if is_sensitive_key(&key)
                && value.len() >= 8
                && !value.contains('/')
                && !value.contains('\\')
            {
                sentinel.add_value(value);
            }
        }
        sentinel
    }

    pub fn redact(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut s = text.to_string();
        for value in &self.values {
            if s.contains(value.as_str()) {
                s = s.replace(value.as_str(), REDACTED);
            }
        }
        let s = redact_pem(&s);
        let s = redact_urls(&s);
        let s = redact_tokens(&s);
        redact_assignments(&s)
    }

    pub fn redact_message(&self, m: &Message) -> Message {
        if !matches!(m.role.as_str(), "user" | "tool") {
            return m.clone();
        }
        Message {
            content: self.redact(&m.content),
            ..m.clone()
        }
    }
}

static GLOBAL: OnceLock<Sentinel> = OnceLock::new();

/// Seed the process-wide sentinel with the provider key before first use, so
/// the exact key is redacted wherever it appears, including a config file.
pub fn seed(api_key: &str) {
    GLOBAL.get_or_init(|| {
        let mut sentinel = Sentinel::from_env();
        sentinel.add_value(api_key);
        sentinel
    });
}

/// The process-wide sentinel, harvested from the environment on first use.
pub fn global() -> &'static Sentinel {
    GLOBAL.get_or_init(Sentinel::from_env)
}

pub fn redact(text: &str) -> String {
    global().redact(text)
}

pub fn redact_message(m: &Message) -> Message {
    global().redact_message(m)
}

fn redact_pem(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(rel) = s[i..].find("-----BEGIN") {
        let start = i + rel;
        let Some(b1) = s[start + 10..].find("-----") else {
            break;
        };
        let begin_end = start + 10 + b1;
        let Some(b2) = s[begin_end + 5..].find("-----END") else {
            break;
        };
        let end_start = begin_end + 5 + b2;
        let Some(b3) = s[end_start + 8..].find("-----") else {
            break;
        };
        let end = end_start + 8 + b3 + 5;
        out.push_str(&s[i..start]);
        out.push_str(REDACTED);
        i = end;
    }
    out.push_str(&s[i..]);
    out
}

fn redact_urls(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let Some(rel) = s[i..].find("://") else {
            out.push_str(&s[i..]);
            break;
        };
        let scheme_end = i + rel + 3;
        out.push_str(&s[i..scheme_end]);
        let mut j = scheme_end;
        while j < b.len() && !is_url_break(b[j]) {
            j += 1;
        }
        let authority = &s[scheme_end..j];
        match authority.find('@') {
            Some(at) if authority[..at].contains(':') => {
                out.push_str(REDACTED);
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        i = j;
    }
    out
}

fn is_url_break(c: u8) -> bool {
    c.is_ascii_whitespace()
        || matches!(
            c,
            b'"' | b'\'' | b'<' | b'>' | b')' | b']' | b'}' | b',' | b'/'
        )
}

fn redact_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = secret_token_len(s, i) {
            out.push_str(REDACTED);
            i += len;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

const PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "rk_test_",
    "pk_live_",
    "pk_test_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "gldt-",
    "glft-",
    "glsoat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xapp-",
    "whsec_",
    "npm_",
    "pypi-",
    "hf_",
    "AIza",
    "AKIA",
    "ASIA",
];

fn secret_token_len(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    if i > 0 {
        let prev = b[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    let rest = &s[i..];
    let len = rest
        .as_bytes()
        .iter()
        .take_while(|c| is_token_byte(**c))
        .count();
    if len == 0 {
        return None;
    }
    let head = &rest[..len];
    if head.starts_with("eyJ") {
        return (len >= MIN_JWT_LEN && head.matches('.').count() >= 2).then_some(len);
    }
    let matched = PREFIXES
        .iter()
        .any(|p| head.len() >= p.len() && head[..p.len()].eq_ignore_ascii_case(p));
    (matched && len >= MIN_TOKEN_LEN).then_some(len)
}

fn is_token_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'+' | b'=')
}

fn redact_assignments(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if is_key_byte(b[i])
            && (i == 0 || !is_key_byte(b[i - 1]))
            && let Some((value_start, value_end, repl)) = match_assignment(s, i)
        {
            out.push_str(&s[i..value_start]);
            out.push_str(&repl);
            i = value_end;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn match_assignment(s: &str, i: usize) -> Option<(usize, usize, String)> {
    let b = s.as_bytes();
    let mut k = i;
    while k < b.len() && is_key_byte(b[k]) {
        k += 1;
    }
    let key = &s[i..k];
    if !is_sensitive_key(key) {
        return None;
    }
    let mut j = k;
    if j < b.len() && matches!(b[j], b'"' | b'\'') {
        j += 1;
    }
    let mut gap = j;
    while gap < b.len() && matches!(b[gap], b' ' | b'\t') {
        gap += 1;
    }
    let value_start = if gap < b.len() && (b[gap] == b'=' || b[gap] == b':') {
        let sep = b[gap];
        let mut v = gap + 1;
        while v < b.len() && matches!(b[v], b' ' | b'\t') {
            v += 1;
        }
        (v, sep)
    } else if gap > j {
        (gap, 0)
    } else {
        return None;
    };
    let (value_start, sep) = value_start;
    if value_start >= b.len() || s[value_start..].starts_with(REDACTED) {
        return None;
    }
    if !matches!(b[value_start], b'"' | b'\'') {
        let token = bare_token(s, value_start);
        let allowed = key.starts_with("--")
            || (sep == b'=' && env_style_key(key))
            || token.eq_ignore_ascii_case("bearer")
            || token.eq_ignore_ascii_case("basic")
            || looks_like_value(token);
        if !allowed {
            return None;
        }
    }
    let (value_end, repl) = redact_value(s, value_start);
    if value_end == value_start {
        return None;
    }
    Some((value_start, value_end, repl))
}

fn bare_token(s: &str, start: usize) -> &str {
    let b = s.as_bytes();
    let mut k = start;
    while k < b.len() && !is_value_break(b[k]) {
        k += 1;
    }
    &s[start..k]
}

/// A whitespace-separated value (no `=`/`:`), such as a `.netrc` password.
/// Gated so prose (`the password is ...`) and code identifiers (`bare_token`)
/// are left alone: a value must carry a digit or a known token prefix.
fn looks_like_value(token: &str) -> bool {
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
    (token.len() >= 6 && has_digit && has_alpha) || starts_with_secret_prefix(token)
}

fn starts_with_secret_prefix(token: &str) -> bool {
    PREFIXES.iter().any(|p| {
        token
            .get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
    })
}

/// An env-style key (`UPPER_CASE`), where a bare value is taken at face value.
fn env_style_key(key: &str) -> bool {
    key.bytes().any(|c| c.is_ascii_alphabetic())
        && key
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// Split an identifier into lowercase words on separators and camelCase, so
/// `KeyCode` is `[key, code]` and `input_tokens` is `[input, tokens]`.
fn key_words(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in raw.chars() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn redact_value(s: &str, start: usize) -> (usize, String) {
    let b = s.as_bytes();
    let quote = b[start];
    if quote == b'"' || quote == b'\'' {
        let mut k = start + 1;
        while k < b.len() && b[k] != quote {
            if b[k] == b'\\' {
                k += 1;
            }
            k += 1;
        }
        let end = (k + 1).min(b.len());
        return (end, format!("{}{REDACTED}{}", quote as char, quote as char));
    }
    let mut k = start;
    while k < b.len() && !is_value_break(b[k]) {
        k += 1;
    }
    if s[start..k].eq_ignore_ascii_case("bearer") || s[start..k].eq_ignore_ascii_case("basic") {
        let mut m = k;
        while m < b.len() && b[m].is_ascii_whitespace() {
            m += 1;
        }
        if s[m..].starts_with(REDACTED) {
            return (m + REDACTED.len(), REDACTED.to_string());
        }
        let mut n = m;
        while n < b.len() && !is_value_break(b[n]) {
            n += 1;
        }
        if n > m {
            return (n, REDACTED.to_string());
        }
    }
    (k, REDACTED.to_string())
}

fn is_value_break(c: u8) -> bool {
    c.is_ascii_whitespace() || matches!(c, b',' | b'}' | b']' | b')' | b';' | b'"' | b'\'' | b'`')
}

fn is_key_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')
}

fn is_sensitive_key(raw: &str) -> bool {
    const WORDS: &[&str] = &[
        "password",
        "passwd",
        "pwd",
        "passphrase",
        "secret",
        "token",
        "credential",
        "creds",
        "auth",
        "authorization",
        "cookie",
        "apikey",
        "privatekey",
        "accesskey",
        "bearer",
    ];
    const COMPOUNDS: &[&str] = &[
        "apikey",
        "privatekey",
        "accesskey",
        "secretkey",
        "signingkey",
        "encryptionkey",
        "clientsecret",
    ];
    if key_words(raw).iter().any(|w| WORDS.contains(&w.as_str())) {
        return true;
    }
    let joined: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    COMPOUNDS.iter().any(|c| joined.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentinel() -> Sentinel {
        let mut s = Sentinel::new();
        s.add_value("hunter2-secret-value");
        s
    }

    #[test]
    fn known_value_is_replaced() {
        assert_eq!(
            sentinel().redact("key is hunter2-secret-value ok"),
            "key is [REDACTED] ok"
        );
    }

    #[test]
    fn known_values_match_longest_first_and_skip_short() {
        let mut s = Sentinel::new();
        s.add_value("abcdef");
        s.add_value("abcdefgh");
        s.add_value("abcdef");
        assert_eq!(s.redact("x abcdefgh y"), "x [REDACTED] y");
        assert_eq!(s.redact("z abcdef w"), "z [REDACTED] w");
        let mut short = Sentinel::new();
        short.add_value("ab");
        assert_eq!(short.redact("ab"), "ab");
    }

    #[test]
    fn pem_block_is_replaced() {
        let text = "a\n-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----\nb";
        assert_eq!(sentinel().redact(text), "a\n[REDACTED]\nb");
    }

    #[test]
    fn url_credentials_are_replaced() {
        assert_eq!(
            sentinel().redact("postgres://user:s3cr3tpw@localhost:5432/db"),
            "postgres://[REDACTED]@localhost:5432/db"
        );
        assert_eq!(
            sentinel().redact("see https://example.com/a:b@c"),
            "see https://example.com/a:b@c"
        );
    }

    #[test]
    fn provider_tokens_are_replaced() {
        assert_eq!(
            sentinel().redact("tok ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 end"),
            "tok [REDACTED] end"
        );
        assert_eq!(
            sentinel().redact("aws AKIAIOSFODNN7EXAMPLE here"),
            "aws [REDACTED] here"
        );
        assert_eq!(
            sentinel().redact("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature123456"),
            "jwt [REDACTED]"
        );
    }

    #[test]
    fn assignments_are_replaced() {
        assert_eq!(
            sentinel().redact("PASSWORD=correct horse"),
            "PASSWORD=[REDACTED] horse"
        );
        assert_eq!(
            sentinel().redact(r#"{"api_key": "sk-abc123", "path": "/x"}"#),
            r#"{"api_key": "[REDACTED]", "path": "/x"}"#
        );
        assert_eq!(
            sentinel().redact("run --token abcdefg --verbose"),
            "run --token [REDACTED] --verbose"
        );
        assert_eq!(
            sentinel().redact("Authorization: Bearer abcdefghijklmnop"),
            "Authorization: [REDACTED]"
        );
    }

    #[test]
    fn space_separated_values_are_replaced() {
        assert_eq!(sentinel().redact("password hunter2"), "password [REDACTED]");
        assert_eq!(
            sentinel().redact("machine x login alice password ghp_abcdefghij"),
            "machine x login alice password [REDACTED]"
        );
        assert_eq!(
            sentinel().redact("the password is required"),
            "the password is required"
        );
    }

    #[test]
    fn basic_auth_is_replaced() {
        assert_eq!(
            sentinel().redact("Authorization: Basic dXNlcjpwYXNz"),
            "Authorization: [REDACTED]"
        );
    }

    #[test]
    fn bearer_credential_is_not_double_redacted() {
        assert_eq!(
            sentinel().redact("Authorization: Bearer ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"),
            "Authorization: [REDACTED]"
        );
        assert_eq!(
            sentinel().redact("Authorization: Basic dXNlcjpwYXNzd29yZA=="),
            "Authorization: [REDACTED]"
        );
    }

    #[test]
    fn empty_assignment_does_not_eat_the_next_line() {
        assert_eq!(
            sentinel().redact("TRANSCRIBE_API_KEY=\n# comentario"),
            "TRANSCRIBE_API_KEY=\n# comentario"
        );
        assert_eq!(
            sentinel().redact("API_KEY=\nport = 8080"),
            "API_KEY=\nport = 8080"
        );
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let text = "let content = compute();\npath=/tmp/file\nkeyboard=us";
        assert_eq!(sentinel().redact(text), text);
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = sentinel().redact("PASSWORD=x sk-abcdefghijklmnopqrstuvwxyz");
        assert_eq!(sentinel().redact(&once), once);
    }
}
