//! Web tools over DeepSeek's Search and Fetch APIs.
//!
//! Both are JSON POSTs through `curlffi`, so there is no HTTP crate in the
//! tree and no page parsing here: the provider answers with the results and
//! the Markdown, and these tools only decide what the model reads.

use crate::curlffi::Easy;
use crate::{Tool, new_tool};
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_void};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const SEARCH_PATH: &str = "/v1/tools/search";
const FETCH_PATH: &str = "/v1/tools/fetch";
const RESULTS: usize = 8;
const MAX_RESULTS: usize = 50;
const MAX_QUERY: usize = 128;
const MAX_URL: usize = 8192;
const CONNECT_TIMEOUT: i64 = 10;
const TRANSFER_TIMEOUT: i64 = 60;
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const MAX_MARKDOWN: usize = 32 * 1024;
const MAX_DETAIL: usize = 300;
const RETRIES: usize = 2;

/// Credentials and endpoint for the two web tools.
///
/// `from_env` reads `DEEPSEEK_API_KEY` and, when set, `DEEPSEEK_BASE_URL`;
/// the base URL carries no `/v1`, which belongs to each endpoint.
#[derive(Clone)]
pub struct Web {
    base_url: String,
    api_key: String,
}

impl Web {
    pub fn new(api_key: impl Into<String>) -> Web {
        Web::at(DEFAULT_BASE_URL, api_key)
    }

    pub fn at(base_url: impl Into<String>, api_key: impl Into<String>) -> Web {
        Web {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    pub fn from_env() -> Option<Web> {
        let api_key = std::env::var("DEEPSEEK_API_KEY").ok()?;
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return None;
        }
        let base_url =
            std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Some(Web::at(base_url, api_key))
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    count: Option<usize>,
}

#[derive(Serialize)]
struct SearchRequest<'a> {
    query: &'a str,
    count: usize,
}

#[derive(Default, Deserialize)]
struct SearchReply {
    #[serde(default)]
    results: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    title: String,
    url: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    site_name: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

#[derive(Serialize)]
struct FetchRequest<'a> {
    url: &'a str,
}

#[derive(Default, Deserialize)]
struct FetchReply {
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
}

struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl Reply {
    fn failure(&self) -> String {
        let why = match self.status {
            400 => "bad request".to_string(),
            402 => "the DeepSeek account has no balance".to_string(),
            408 => "the request timed out".to_string(),
            413 => "the request body is too large".to_string(),
            422 => "DeepSeek could not read the page".to_string(),
            429 => "rate limited: 5 requests per second per account".to_string(),
            500 => "DeepSeek had an internal error".to_string(),
            504 => "DeepSeek timed out downstream".to_string(),
            other if other >= 500 => "DeepSeek had a server error".to_string(),
            other => format!("unexpected status {other}"),
        };
        match message(&self.body) {
            Some(detail) => format!("{why}: {detail}"),
            None => why,
        }
    }
}

fn retryable(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

/// The message of the provider's error body, when there is one.
fn message(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let raw = value
        .get("error")
        .and_then(|error| error.get("message"))
        .or_else(|| value.get("message"))
        .and_then(|message| message.as_str())?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if raw.is_empty() {
        return None;
    }
    Some(raw.chars().take(MAX_DETAIL).collect())
}

impl Web {
    fn post(&self, path: &str, body: &[u8]) -> Result<Reply, String> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let mut last = String::new();
        for attempt in 0..=RETRIES {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(500 << (attempt - 1)));
            }
            match self.post_once(&url, body) {
                Ok(reply) if !(200..300).contains(&reply.status) => {
                    let failure = reply.failure();
                    if !retryable(reply.status) {
                        return Err(failure);
                    }
                    last = failure;
                }
                Ok(reply) => return Ok(reply),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn post_once(&self, url: &str, body: &[u8]) -> Result<Reply, String> {
        let mut easy = Easy::new()?;
        easy.url(url)?;
        easy.post()?;
        easy.post_fields(body)?;
        easy.fail_on_error(false)?;
        easy.connect_timeout(CONNECT_TIMEOUT)?;
        easy.timeout(TRANSFER_TIMEOUT)?;
        easy.low_speed(1, 30)?;
        easy.headers(&[
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
        ])?;

        let mut collector = Collector {
            body: Vec::new(),
            overflow: false,
        };
        let performed = {
            let mut transfer = easy.transfer();
            transfer.write_function(collect, &mut collector as *mut Collector as *mut c_void);
            transfer.perform()
        };
        if collector.overflow {
            return Err(format!(
                "response exceeds {} MiB",
                MAX_RESPONSE / (1024 * 1024)
            ));
        }
        performed?;
        Ok(Reply {
            status: easy.response_code()? as u16,
            body: collector.body,
        })
    }
}

struct Collector {
    body: Vec<u8>,
    overflow: bool,
}

unsafe extern "C" fn collect(
    chunk: *mut c_char,
    size: usize,
    count: usize,
    data: *mut c_void,
) -> usize {
    let len = size * count;
    let collector = unsafe { &mut *(data as *mut Collector) };
    if collector.body.len() + len > MAX_RESPONSE {
        collector.overflow = true;
        return 0;
    }
    let slice = unsafe { std::slice::from_raw_parts(chunk as *const u8, len) };
    collector.body.extend_from_slice(slice);
    len
}

pub fn search(web: Web) -> Tool {
    let mut tool = new_tool(
        "search",
        "Search the web with DeepSeek's search engine. Returns a numbered list of results with title, URL, a summary read by models and, when known, site and date. Read one with fetch.",
        r#"{"type":"object","properties":{"query":{"type":"string","description":"Search query, 1 to 128 characters. Supports site:, \"exact phrase\", -term, -site:, lang:, after: and before:"},"count":{"type":"integer","description":"How many results to return (default 8, max 50)"}},"required":["query"]}"#,
        move |args: SearchArgs| {
            let query = args.query.trim();
            let length = query.chars().count();
            if length == 0 || length > MAX_QUERY {
                return format!("error: the query must have 1 to {MAX_QUERY} characters");
            }
            let count = args.count.unwrap_or(RESULTS).clamp(1, MAX_RESULTS);
            let body = match serde_json::to_vec(&SearchRequest { query, count }) {
                Ok(body) => body,
                Err(error) => return format!("error: {error}"),
            };
            match web.post(SEARCH_PATH, &body) {
                Ok(reply) => match serde_json::from_slice::<SearchReply>(&reply.body) {
                    Ok(results) => format_results(&results.results),
                    Err(error) => format!("error: unexpected search response: {error}"),
                },
                Err(error) => format!("error: {error}"),
            }
        },
    );
    tool.snippet = "Search the web (DeepSeek): 1 to 50 results with summary and date";
    tool
}

fn format_results(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no results".to_string();
    }
    let mut out = String::new();
    for (index, hit) in hits.iter().enumerate() {
        let title = hit.title.trim();
        let mut meta = Vec::new();
        if let Some(site) = hit.site_name.as_deref().map(str::trim)
            && !site.is_empty()
        {
            meta.push(site.to_string());
        }
        if let Some(date) = hit.date.as_deref().map(str::trim)
            && !date.is_empty()
        {
            meta.push(date.split('T').next().unwrap_or(date).to_string());
        }
        let meta = if meta.is_empty() {
            String::new()
        } else {
            format!(" ({})", meta.join(", "))
        };
        out.push_str(&format!(
            "{}. {title}{meta}\n   {}\n",
            index + 1,
            hit.url.trim()
        ));
        let summary = if hit.summary.trim().is_empty() {
            hit.snippet.trim()
        } else {
            hit.summary.trim()
        };
        let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
        if !summary.is_empty() {
            out.push_str(&format!("   {summary}\n"));
        }
    }
    out.trim_end().to_string()
}

pub fn fetch(web: Web) -> Tool {
    let mut tool = new_tool(
        "fetch",
        "Fetch a URL through DeepSeek and return its title and content as Markdown, without the chrome, the navigation or the scripts.",
        r#"{"type":"object","properties":{"url":{"type":"string","description":"HTTP or HTTPS URL"}},"required":["url"]}"#,
        move |args: FetchArgs| {
            let url = args.url.trim();
            if let Err(error) = check_url(url) {
                return format!("error: {error}");
            }
            let body = match serde_json::to_vec(&FetchRequest { url }) {
                Ok(body) => body,
                Err(error) => return format!("error: {error}"),
            };
            match web.post(FETCH_PATH, &body) {
                Ok(reply) => match serde_json::from_slice::<FetchReply>(&reply.body) {
                    Ok(page) => compose(&page),
                    Err(error) => format!("error: unexpected fetch response: {error}"),
                },
                Err(error) => format!("error: {error}"),
            }
        },
    );
    tool.snippet = "Fetch a URL as Markdown (DeepSeek)";
    tool
}

fn check_url(url: &str) -> Result<(), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("the url must start with http:// or https://".to_string());
    }
    if url.chars().count() > MAX_URL {
        return Err(format!("the url is longer than {MAX_URL} characters"));
    }
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err("the url has no host".to_string());
    }
    if authority.contains('@') {
        return Err("the url must not carry a username or password".to_string());
    }
    Ok(())
}

fn compose(page: &FetchReply) -> String {
    let content = page.content.trim();
    if content.is_empty() {
        return "the page has no readable content".to_string();
    }
    let title = page.title.trim();
    let text = if title.is_empty() {
        content.to_string()
    } else {
        format!("# {title}\n\n{content}")
    };
    truncate(text)
}

fn truncate(text: String) -> String {
    if text.chars().count() <= MAX_MARKDOWN {
        return text;
    }
    let mut out: String = text.chars().take(MAX_MARKDOWN).collect();
    out.push_str("\n\n[truncado]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A local server that answers each request with the next canned reply
    /// and hands back what it received.
    fn serve(replies: Vec<(u16, &str)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let replies: Vec<(u16, String)> = replies
            .into_iter()
            .map(|(status, body)| (status, body.to_string()))
            .collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let reason = match status {
                    200 => "OK",
                    402 => "Payment Required",
                    422 => "Unprocessable Entity",
                    429 => "Too Many Requests",
                    502 => "Bad Gateway",
                    _ => "Error",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            requests
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buffer.len() < end + 4 + length {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    fn run(tool: &Tool, args: &str) -> String {
        (tool.run)(args, &mut |_| {}).text
    }

    fn body_of(request: &str) -> &str {
        request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap()
    }

    #[test]
    fn search_posts_the_query_and_formats_the_hits() {
        let (base, server) = serve(vec![(
            200,
            r#"{"results":[
                {"title":"Rust","url":"https://rust-lang.org/","snippet":"short","summary":"A long summary about Rust.","site_name":"rust-lang","date":"2026-06-01T00:00:00+00:00"},
                {"title":"Docs","url":"https://doc.rust-lang.org/","snippet":"the book","summary":"","site_name":null,"date":null}
            ]}"#,
        )]);
        let text = run(
            &search(Web::at(&base, "k")),
            r#"{"query":"rust","count":2}"#,
        );
        let requests = server.join().unwrap();

        assert!(
            requests[0].starts_with("POST /v1/tools/search HTTP/1.1"),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("Authorization: Bearer k"));
        assert_eq!(body_of(&requests[0]), r#"{"query":"rust","count":2}"#);
        assert!(
            text.starts_with("1. Rust (rust-lang, 2026-06-01)"),
            "{text}"
        );
        assert!(text.contains("   https://rust-lang.org/"));
        assert!(text.contains("   A long summary about Rust."));
        assert!(
            text.contains("2. Docs\n   https://doc.rust-lang.org/"),
            "{text}"
        );
        assert!(text.contains("   the book"));
    }

    #[test]
    fn an_empty_result_list_says_so() {
        let (base, server) = serve(vec![(200, r#"{"results":[]}"#)]);
        assert_eq!(
            run(&search(Web::at(&base, "k")), r#"{"query":"x"}"#),
            "no results"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_query_outside_the_limits_never_reaches_the_network() {
        let tool = search(Web::new("k"));
        let long = "a".repeat(MAX_QUERY + 1);
        let text = run(&tool, &format!(r#"{{"query":"{long}"}}"#));
        assert!(
            text.starts_with("error: the query must have 1 to 128"),
            "{text}"
        );
        let text = run(&tool, r#"{"query":"   "}"#);
        assert!(text.starts_with("error:"), "{text}");
    }

    #[test]
    fn fetch_posts_the_url_and_returns_the_markdown() {
        let (base, server) = serve(vec![(
            200,
            r#"{"title":"Axe","content":"The loop is the whole thing."}"#,
        )]);
        let text = run(
            &fetch(Web::at(&base, "k")),
            r#"{"url":"https://example.com/axe"}"#,
        );
        let requests = server.join().unwrap();

        assert!(
            requests[0].starts_with("POST /v1/tools/fetch HTTP/1.1"),
            "{}",
            requests[0]
        );
        assert_eq!(
            body_of(&requests[0]),
            r#"{"url":"https://example.com/axe"}"#
        );
        assert_eq!(text, "# Axe\n\nThe loop is the whole thing.");
    }

    #[test]
    fn fetch_truncates_a_page_that_is_too_long() {
        let body = "x".repeat(MAX_MARKDOWN + 100);
        let (base, server) = serve(vec![(
            200,
            &format!(r#"{{"title":"T","content":"{body}"}}"#),
        )]);
        let text = run(
            &fetch(Web::at(&base, "k")),
            r#"{"url":"https://example.com/"}"#,
        );
        server.join().unwrap();
        assert!(text.ends_with("[truncado]"), "{}", text.len());
        assert_eq!(text.chars().count(), MAX_MARKDOWN + 12);
    }

    #[test]
    fn fetch_rejects_a_url_with_credentials() {
        let tool = fetch(Web::new("k"));
        let text = run(&tool, r#"{"url":"https://user:pass@example.com/"}"#);
        assert!(text.contains("username or password"), "{text}");
        let text = run(&tool, r#"{"url":"ftp://example.com/"}"#);
        assert!(text.starts_with("error:"), "{text}");
        let text = run(&tool, r#"{"url":"https://"}"#);
        assert!(text.contains("no host"), "{text}");
    }

    #[test]
    fn a_refused_page_reports_why() {
        let (base, server) = serve(vec![(
            422,
            r#"{"error":{"message":"could not retrieve usable page content","type":"invalid_request_error"}}"#,
        )]);
        let text = run(
            &fetch(Web::at(&base, "k")),
            r#"{"url":"https://example.com/missing"}"#,
        );
        server.join().unwrap();
        assert_eq!(
            text,
            "error: DeepSeek could not read the page: could not retrieve usable page content"
        );
    }

    #[test]
    fn a_rate_limited_search_is_retried() {
        let (base, server) = serve(vec![
            (429, r#"{"error":{"message":"rate limit exceeded"}}"#),
            (
                200,
                r#"{"results":[{"title":"T","url":"https://t.example/"}]}"#,
            ),
        ]);
        let text = run(&search(Web::at(&base, "k")), r#"{"query":"x"}"#);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(text.starts_with("1. T"), "{text}");
    }

    #[test]
    fn a_broken_gateway_gives_up_after_the_retries() {
        let (base, server) = serve(vec![
            (502, "{}"),
            (502, "{}"),
            (502, r#"{"error":{"message":"upstream is down"}}"#),
        ]);
        let text = run(&search(Web::at(&base, "k")), r#"{"query":"x"}"#);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), RETRIES + 1);
        assert_eq!(text, "error: DeepSeek had a server error: upstream is down");
    }
}
