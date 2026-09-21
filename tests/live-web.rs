//! Live web tool tests. They hit the network and, for the rendering one, a
//! headless browser, so they are ignored by default:
//!
//!     cargo test --test live-web -- --ignored --nocapture

use axe::tui::build_tools;
use std::io::{Read, Write};
use std::net::TcpListener;

fn run(name: &str, args: &str) -> String {
    let tools = build_tools("/tmp");
    let tool = tools.iter().find(|tool| tool.name == name).unwrap();
    let output = (tool.run)(args, &mut |_| {});
    output.text.clone()
}

#[test]
#[ignore]
fn searches_the_web() {
    let text = run("search", r#"{"query":"rust programming language"}"#);
    println!("{text}");
    assert!(text.contains("1. "), "{text}");
    assert!(text.contains("http"), "{text}");
    assert!(!text.starts_with("error"), "{text}");
}

#[test]
#[ignore]
fn fetches_and_extracts_a_page() {
    let text = run(
        "fetch",
        r#"{"url":"https://emschwartz.me/comparing-13-rust-crates-for-extracting-text-from-html/"}"#,
    );
    println!("{}", text.chars().take(600).collect::<String>());
    println!("... {} caracteres", text.chars().count());
    assert!(text.contains("dom_smoothie"), "{text}");
    assert!(!text.contains("<div"), "quedó html crudo: {text}");
    assert!(!text.starts_with("error"), "{text}");
}

#[test]
#[ignore]
fn renders_a_page_that_only_exists_after_javascript() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for _ in 0..4 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let body = r#"<!doctype html><html><head><title>App</title></head><body>
                <div id="root">cargando</div>
                <script>
                  document.getElementById('root').innerHTML =
                    '<article><h1>Renderizado</h1>' +
                    '<p>' + 'contenido que solo existe despues de correr el javascript. '.repeat(8) +
                    '</p></article>';
                </script></body></html>"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });

    let text = run("fetch", &format!(r#"{{"url":"http://127.0.0.1:{port}/"}}"#));
    println!("{}", text.chars().take(300).collect::<String>());
    server.join().ok().unwrap();
    assert!(text.contains("Renderizado"), "{text}");
    assert!(text.contains("despues de correr el javascript"), "{text}");
}
