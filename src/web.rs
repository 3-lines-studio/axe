//! Web tools: a DuckDuckGo search and a page fetcher.
//!
//! Both ride on `curlffi`, so there is no HTTP crate in the tree, and both
//! stay in process. The fetcher reads the page, hands it to Readability and
//! converts what survives to Markdown; only when almost no text comes out
//! does it pay for a headless browser, and only if there is one installed.

use crate::curlffi::Easy;
use crate::{Tool, new_tool};
use serde::Deserialize;
use std::ffi::{c_char, c_void};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SEARCH_UA: &str = "Mozilla/5.0 (X11; Linux x86_64)";
const FETCH_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";
const SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const RESULTS: usize = 8;
const CONNECT_TIMEOUT: i64 = 10;
const TRANSFER_TIMEOUT: i64 = 30;
const MAX_SEARCH: usize = 2 * 1024 * 1024;
const MAX_PAGE: usize = 8 * 1024 * 1024;
const MAX_MARKDOWN: usize = 32 * 1024;
const MAX_LINKS: usize = 40;
const MIN_TEXT: usize = 200;
const RENDER_BUDGET_MS: u64 = 4000;
const RENDER_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSERS: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
];

struct Page {
    status: u32,
    content_type: String,
    url: String,
    body: Vec<u8>,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    count: Option<usize>,
}

pub fn search() -> Tool {
    let mut tool = new_tool(
        "search",
        "Search the web. Returns a numbered list of results with title, URL and snippet; read one with fetch.",
        r#"{"type":"object","properties":{"query":{"type":"string","description":"Search query"},"count":{"type":"integer","description":"How many results to return (default 8)"}},"required":["query"]}"#,
        |a: SearchArgs| {
            let query = a.query.trim();
            if query.is_empty() {
                return "error: empty query".to_string();
            }
            let count = a.count.unwrap_or(RESULTS).clamp(1, RESULTS * 4);
            match run_search(query) {
                Ok(items) => format_results(&items, count),
                Err(error) => format!("error: {error}"),
            }
        },
    );
    tool.snippet = "Search the web (DuckDuckGo)";
    tool
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

pub fn fetch() -> Tool {
    let mut tool = new_tool(
        "fetch",
        "Fetch a URL and return it as Markdown. HTML goes through readability extraction, so you get the content and not the chrome. Pages that need JavaScript are rendered when a browser is available. The links found on the page are listed at the end.",
        r#"{"type":"object","properties":{"url":{"type":"string","description":"HTTP or HTTPS URL"}},"required":["url"]}"#,
        |a: FetchArgs| {
            let url = a.url.trim();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return format!("error: url must start with http:// or https://, got {url}");
            }
            match run_fetch(url) {
                Ok(text) => text,
                Err(error) => format!("error: {error}"),
            }
        },
    );
    tool.snippet =
        "Fetch a URL as Markdown, rendering JavaScript pages when a browser is installed";
    tool
}

fn run_search(query: &str) -> Result<Vec<Item>, String> {
    let url = format!("{SEARCH_ENDPOINT}?q={}", encode(query));
    let page = get(&url, MAX_SEARCH, SEARCH_UA)?;
    check_status(&page)?;
    Ok(parse(&decode(&page.body, &page.content_type)))
}

fn format_results(items: &[Item], count: usize) -> String {
    if items.is_empty() {
        return "no results".to_string();
    }
    let mut out = String::new();
    for (index, item) in items.iter().take(count).enumerate() {
        let snippet = item
            .snippet
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("{}. {}\n   {}\n", index + 1, item.title, item.url));
        if !snippet.is_empty() {
            out.push_str(&format!("   {snippet}\n"));
        }
    }
    out.trim_end().to_string()
}

fn run_fetch(url: &str) -> Result<String, String> {
    let page = get(url, MAX_PAGE, FETCH_UA)?;
    check_status(&page)?;
    if !is_html(&page.content_type) {
        return plain_or_error(&page);
    }

    let mut html = decode(&page.body, &page.content_type);
    let (mut title, mut markdown) = extract(&html, &page.url)?;
    if markdown.chars().count() < MIN_TEXT
        && let Some(rendered) = render(url)
        && let Ok((rendered_title, rendered_markdown)) = extract(&rendered, &page.url)
        && !rendered_markdown.trim().is_empty()
    {
        title = rendered_title;
        markdown = rendered_markdown;
        html = rendered;
    }
    Ok(compose(&title, &markdown, &page_links(&html, &page.url)))
}

/// Readability first, then Markdown. Returns the article title and body.
fn extract(html: &str, url: &str) -> Result<(String, String), String> {
    let mut reader =
        dom_smoothie::Readability::new(html, Some(url), None).map_err(|error| error.to_string())?;
    let article = reader.parse().map_err(|error| error.to_string())?;
    let markdown = htmd::convert(article.content.as_ref()).map_err(|error| error.to_string())?;
    Ok((article.title, markdown))
}

fn compose(title: &str, markdown: &str, links: &[String]) -> String {
    let mut out = String::new();
    if !title.trim().is_empty() {
        out.push_str(&format!("# {}\n\n", title.trim()));
    }
    out.push_str(markdown.trim());
    if out.chars().count() > MAX_MARKDOWN {
        out = out.chars().take(MAX_MARKDOWN).collect();
        out.push_str("\n\n[truncado]");
    }
    if !links.is_empty() {
        out.push_str("\n\n## Links");
        for link in links {
            out.push_str(&format!("\n- <{link}>"));
        }
    }
    if out.trim().is_empty() {
        return "the page has no readable content".to_string();
    }
    out
}

/// Every distinct link on the page, navigation included: readability throws
/// the menu away, which is exactly where the way to the next page lives.
fn page_links(html: &str, base: &str) -> Vec<String> {
    let document = dom_query::Document::from(html);
    let mut seen = std::collections::HashSet::new();
    let mut links = Vec::new();
    for node in document.select("a[href]").iter() {
        let Some(href) = node.attr("href") else {
            continue;
        };
        let Some(url) = absolutize(base, href.trim()) else {
            continue;
        };
        if seen.insert(url.clone()) {
            links.push(url);
        }
        if links.len() == MAX_LINKS {
            break;
        }
    }
    links
}

/// Enough URL joining for the shapes HTML links come in. Fragments, other
/// schemes and `..` are skipped rather than guessed.
fn absolutize(base: &str, href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let scheme_end = base.find("://")? + 3;
    let origin_end = base[scheme_end..]
        .find('/')
        .map_or(base.len(), |at| scheme_end + at);
    let (origin, path) = base.split_at(origin_end);
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{}://{rest}", &base[..scheme_end - 3]));
    }
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    if href.is_empty() || href.starts_with('#') || href.starts_with('?') {
        return None;
    }
    if href.contains("..") || href.contains(':') {
        return None;
    }
    let dir = path.rfind('/').map_or("/", |at| &path[..=at]);
    Some(format!("{origin}{dir}{href}"))
}

fn plain_or_error(page: &Page) -> Result<String, String> {
    if page.content_type.starts_with("text/") {
        let text = decode(&page.body, &page.content_type);
        return Ok(truncate(text.trim()));
    }
    Err(format!(
        "unsupported content type {}",
        if page.content_type.is_empty() {
            "unknown".to_string()
        } else {
            page.content_type.clone()
        }
    ))
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_MARKDOWN {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_MARKDOWN).collect();
    out.push_str("\n\n[truncado]");
    out
}

fn is_html(content_type: &str) -> bool {
    content_type.is_empty()
        || content_type.starts_with("text/html")
        || content_type.starts_with("application/xhtml")
}

fn check_status(page: &Page) -> Result<(), String> {
    if (200..300).contains(&page.status) {
        return Ok(());
    }
    Err(format!(
        "HTTP {} from {}",
        page.status,
        if page.url.is_empty() {
            "the server"
        } else {
            &page.url
        }
    ))
}

/// A headless browser, if one is installed, printing the DOM after scripts
/// ran. Chromium's own flags do the waiting; no CDP client involved.
fn render(url: &str) -> Option<String> {
    let browser = which(BROWSERS)?;
    let mut command = std::process::Command::new(browser);
    command
        .arg("--headless")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg(format!("--virtual-time-budget={RENDER_BUDGET_MS}"))
        .arg("--dump-dom");
    if unsafe { libc::geteuid() } == 0 {
        command.arg("--no-sandbox");
    }
    let mut child = command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Drain the pipe while the browser works: a DOM bigger than the pipe
    // buffer would block the child instead of letting it exit.
    let reader = std::thread::spawn(move || {
        let mut body = Vec::new();
        let _ = stdout.by_ref().take(MAX_PAGE as u64).read_to_end(&mut body);
        body
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < RENDER_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let body = reader.join().ok()?;
    if !status.success() || body.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&body).into_owned())
}

fn which(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// GET with redirects and transparent decompression, capped at `limit` bytes.
///
/// The user agent matters: DuckDuckGo answers a browser-shaped one with its
/// anti-bot page unless the rest of the fingerprint matches too.
fn get(url: &str, limit: usize, user_agent: &str) -> Result<Page, String> {
    let mut easy = Easy::new()?;
    easy.url(url)?;
    easy.follow_location(true)?;
    easy.max_redirects(5)?;
    easy.user_agent(user_agent)?;
    easy.accept_encoding("")?;
    easy.connect_timeout(CONNECT_TIMEOUT)?;
    easy.timeout(TRANSFER_TIMEOUT)?;

    let mut collector = Collector {
        body: Vec::new(),
        limit,
        overflow: false,
    };
    let performed = {
        let mut transfer = easy.transfer();
        transfer.write_function(collect, &mut collector as *mut Collector as *mut c_void);
        transfer.perform()
    };
    if collector.overflow {
        return Err(format!("response exceeds {} KiB", limit / 1024));
    }
    performed?;

    Ok(Page {
        status: easy.response_code()?,
        content_type: easy.content_type().unwrap_or_default(),
        url: easy.effective_url().unwrap_or_else(|_| url.to_string()),
        body: collector.body,
    })
}

struct Collector {
    body: Vec<u8>,
    limit: usize,
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
    if collector.body.len() + len > collector.limit {
        collector.overflow = true;
        return 0;
    }
    let slice = unsafe { std::slice::from_raw_parts(chunk as *const u8, len) };
    collector.body.extend_from_slice(slice);
    len
}

fn decode(body: &[u8], content_type: &str) -> String {
    let encoding = charset(content_type)
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    text.into_owned()
}

fn charset(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_string())
    })
}

fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(PartialEq)]
struct Item {
    url: String,
    title: String,
    snippet: String,
}

enum Mode {
    Idle,
    Title,
    Snippet,
}

/// DuckDuckGo's HTML view: one `a.result__a` per hit, one `a.result__snippet`
/// after it, both linking through a `uddg=` redirector.
fn parse(html: &str) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let mut mode = Mode::Idle;
    let mut title = String::new();
    let mut href = String::new();
    let mut rest = html;

    while let Some(open) = rest.find('<') {
        let text = decode_entities(&rest[..open]);
        match mode {
            Mode::Title => title.push_str(&text),
            Mode::Snippet => {
                if let Some(item) = items.last_mut() {
                    item.snippet.push_str(&text);
                }
            }
            Mode::Idle => {}
        }

        rest = &rest[open..];
        let Some(close) = rest.find('>') else {
            break;
        };
        classify(
            &rest[1..close],
            &mut items,
            &mut mode,
            &mut title,
            &mut href,
        );
        rest = &rest[close + 1..];
    }

    items
}

fn classify(
    tag: &str,
    items: &mut Vec<Item>,
    mode: &mut Mode,
    title: &mut String,
    href: &mut String,
) {
    if !tag_name(tag).eq_ignore_ascii_case("a") {
        return;
    }

    if tag.starts_with('/') {
        match mode {
            Mode::Title => {
                if !href.is_empty() {
                    items.push(Item {
                        url: real_url(href),
                        title: title.trim().to_string(),
                        snippet: String::new(),
                    });
                }
                *mode = Mode::Idle;
            }
            Mode::Snippet => *mode = Mode::Idle,
            Mode::Idle => {}
        }
        return;
    }

    let class = attr(tag, "class").unwrap_or_default();
    if class.split_whitespace().any(|token| token == "result__a") {
        *href = attr(tag, "href").unwrap_or_default().to_string();
        title.clear();
        *mode = Mode::Title;
    } else if class
        .split_whitespace()
        .any(|token| token == "result__snippet")
    {
        *mode = Mode::Snippet;
    }
}

fn real_url(href: &str) -> String {
    let href = match href.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => href.to_string(),
    };
    match href.find("uddg=") {
        Some(at) => percent_decode(href[at + 5..].split('&').next().unwrap_or("")),
        None => href,
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn tag_name(tag: &str) -> &str {
    let tag = tag.strip_prefix('/').unwrap_or(tag);
    match tag.find(|c: char| c.is_whitespace() || c == '/') {
        Some(end) => &tag[..end],
        None => tag,
    }
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    loop {
        let at = find_attr(rest, name)?;
        rest = &rest[at + name.len()..];
        let value = match rest.trim_start().strip_prefix('=') {
            Some(value) => value.trim_start(),
            None => continue,
        };
        let quote = value.chars().next()?;
        if quote == '"' || quote == '\'' {
            let end = value[1..].find(quote)?;
            return Some(&value[1..1 + end]);
        }
        return Some(value.split_whitespace().next().unwrap_or(""));
    }
}

fn find_attr(rest: &str, name: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut start = 0;
    while let Some(offset) = rest[start..].find(name) {
        let at = start + offset;
        let before = at == 0 || bytes[at - 1].is_ascii_whitespace();
        let after = bytes
            .get(at + name.len())
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'=');
        if before && after {
            return Some(at);
        }
        start = at + name.len();
    }
    None
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        match tail.find(';').filter(|end| *end <= 12) {
            Some(end) => match entity_char(&tail[1..end]) {
                Some(c) => {
                    out.push(c);
                    rest = &tail[end + 1..];
                }
                None => {
                    out.push('&');
                    rest = &tail[1..];
                }
            },
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }

    out.push_str(rest);
    out
}

fn entity_char(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
            }
            let decimal = entity.strip_prefix('#')?;
            decimal.parse::<u32>().ok().and_then(char::from_u32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULTS_HTML: &str = r#"
    <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=abc">The <b>Rust</b> Programming Language</a>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=abc"><b>Rust</b> is fast &amp; reliable.</a>
    <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&amp;rut=def">Example</a>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F">Snippet   with
    newlines.</a>
    "#;

    #[test]
    fn parses_results() {
        let items = parse(RESULTS_HTML);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url, "https://rust-lang.org/");
        assert_eq!(items[0].title, "The Rust Programming Language");
        assert_eq!(items[0].snippet.trim(), "Rust is fast & reliable.");
        assert_eq!(items[1].url, "https://example.com/a?b=1");
        assert_eq!(items[1].title, "Example");
    }

    #[test]
    fn formats_results() {
        let text = format_results(&parse(RESULTS_HTML), RESULTS);
        assert!(text.starts_with("1. The Rust Programming Language\n   https://rust-lang.org/"));
        assert!(text.contains("2. Example\n   https://example.com/a?b=1"));
        assert_eq!(format_results(&parse(RESULTS_HTML), 1).lines().count(), 3);
    }

    #[test]
    fn joins_the_url_shapes_html_uses() {
        let base = "https://example.com/a/b/index.html";
        assert_eq!(
            absolutize(base, "https://x.com/y").unwrap(),
            "https://x.com/y"
        );
        assert_eq!(
            absolutize(base, "//cdn.com/z").unwrap(),
            "https://cdn.com/z"
        );
        assert_eq!(
            absolutize(base, "/docs/x").unwrap(),
            "https://example.com/docs/x"
        );
        assert_eq!(
            absolutize(base, "sub.html").unwrap(),
            "https://example.com/a/b/sub.html"
        );
        assert_eq!(
            absolutize("http://127.0.0.1:8080", "x").unwrap(),
            "http://127.0.0.1:8080/x"
        );
        for skipped in [
            "#frag",
            "?q=1",
            "mailto:a@b.com",
            "../up",
            "javascript:void(0)",
        ] {
            assert!(absolutize(base, skipped).is_none(), "{skipped}");
        }
    }

    #[test]
    fn lists_links_once_in_page_order() {
        let html = r##"<html><body><nav>
            <a href="/docs/a">A</a><a href="/docs/a">A otra vez</a>
            <a href="#top">Arriba</a><a href="https://x.com/y">Y</a>
            </nav><article><p>cuerpo</p></article></body></html>"##;
        let links = page_links(html, "https://example.com/guia/index.html");
        assert_eq!(links, ["https://example.com/docs/a", "https://x.com/y"]);
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(
            decode_entities("a &amp; b &#39;c&#x27; &lt;d&gt;"),
            "a & b 'c' <d>"
        );
    }

    #[test]
    fn reads_the_declared_charset() {
        assert_eq!(
            charset("text/html; charset=ISO-8859-1").as_deref(),
            Some("ISO-8859-1")
        );
        assert_eq!(
            charset("text/html; charset=\"utf-8\"").as_deref(),
            Some("utf-8")
        );
        assert_eq!(charset("text/html"), None);
    }

    #[test]
    fn extracts_the_article_and_drops_the_chrome() {
        let html = r#"<html><head><title>Axe</title></head><body>
            <nav>menu menu menu</nav>
            <article><h1>Real heading</h1><p>This is the body of the article, long enough to be
            chosen as the main content of the document by the readability scoring pass, which
            needs a reasonable amount of prose to work with.</p>
            <p>Second paragraph with a <a href="https://example.com/x">link</a>.</p></article>
            <footer>footer footer</footer></body></html>"#;
        let (title, markdown) = extract(html, "https://example.com/").unwrap();
        assert_eq!(title, "Axe");
        assert!(markdown.contains("Real heading"), "{markdown}");
        assert!(
            markdown.contains("[link](https://example.com/x)"),
            "{markdown}"
        );
        assert!(!markdown.contains("menu menu"), "{markdown}");
        assert!(!markdown.contains("footer"), "{markdown}");
    }

    #[test]
    fn composes_with_the_title_and_truncates() {
        assert_eq!(compose("T", "body", &[]), "# T\n\nbody");
        assert_eq!(compose("", "body", &[]), "body");
        assert!(compose("T", &"x".repeat(MAX_MARKDOWN + 10), &[]).ends_with("[truncado]"));
        assert_eq!(
            compose("T", "body", &["https://a".to_string()]),
            "# T\n\nbody\n\n## Links\n- <https://a>"
        );
    }
}
