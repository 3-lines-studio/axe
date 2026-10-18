use axe::tools::build_tools_with;
use axe::web::Web;

fn run(name: &str, args: &str) -> String {
    let web = Web::from_env().expect("DEEPSEEK_API_KEY");
    let tools = build_tools_with(
        std::sync::Arc::new(axe::machine::Local::new("/tmp")),
        Some(web),
    );
    let tool = tools.iter().find(|tool| tool.name == name).unwrap();
    let output = (tool.run)(args, &mut |_| {});
    output.text.clone()
}

#[test]
#[ignore]
fn searches_the_web() {
    let text = run(
        "search",
        r#"{"query":"DeepSeek search api site:api-docs.deepseek.com","count":3}"#,
    );
    println!("{text}");
    assert!(text.starts_with("1. "), "{text}");
    assert!(text.contains("https://"), "{text}");
    assert!(!text.starts_with("error"), "{text}");
}

#[test]
#[ignore]
fn fetches_a_page_as_markdown() {
    let text = run(
        "fetch",
        r#"{"url":"https://api-docs.deepseek.com/guides/search"}"#,
    );
    println!("{}", text.chars().take(600).collect::<String>());
    println!("... {} caracteres", text.chars().count());
    assert!(text.starts_with("# "), "sin título en markdown: {text}");
    assert!(text.contains("v1/tools/search"), "{text}");
    assert!(!text.contains("<div"), "quedó html crudo: {text}");
    assert!(!text.starts_with("error"), "{text}");
}

#[test]
#[ignore]
fn a_missing_page_reports_it() {
    let text = run(
        "fetch",
        r#"{"url":"https://example.com/definitely-not-here-axe"}"#,
    );
    println!("{text}");
    assert!(text.starts_with("error: "), "{text}");
}

#[test]
#[ignore]
fn a_vague_query_gets_a_weak_matches_hint() {
    let text = run("search", r#"{"query":"auto","count":3}"#);
    println!("{text}");
    assert!(text.contains("weak matches"), "{text}");
}

#[test]
#[ignore]
fn a_precise_query_gets_no_hint() {
    let text = run(
        "search",
        r#"{"query":"deepseek flash pricing site:api-docs.deepseek.com","count":3}"#,
    );
    println!("{text}");
    assert!(!text.contains("weak matches"), "{text}");
}
