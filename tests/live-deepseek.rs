use axe::{Message, OpenAI, Provider, Request};

fn provider() -> OpenAI {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY");
    let base = std::env::var("AXE_BASE").unwrap_or_else(|_| "https://api.deepseek.com".into());
    OpenAI::new(base, key)
}

fn padding(turns: usize) -> String {
    "Rellena el contexto con texto estable para que el prefijo sea cacheable. ".repeat(turns)
}

#[test]
#[ignore]
fn the_effort_limits_and_user_id_are_accepted() {
    let messages = vec![Message {
        role: "user".into(),
        content: "di hola".into(),
        ..Default::default()
    }];
    let provider = provider()
        .effort("low")
        .max_tokens(512)
        .user_id("axe-live-test");
    let response = provider
        .complete(&Request {
            model: "deepseek-flash",
            system: "",
            messages: &messages,
            tools: &[],
        })
        .expect("complete");
    println!("{response:?}");
    assert!(!response.message.content.trim().is_empty());
    assert!(response.usage.output > 0);
}

#[test]
#[ignore]
fn a_repeated_prefix_hits_the_context_cache() {
    let messages = vec![Message {
        role: "user".into(),
        content: "di hola".into(),
        ..Default::default()
    }];
    let provider = provider().effort("low").max_tokens(64);
    let request = Request {
        model: "deepseek-flash",
        system: &padding(400),
        messages: &messages,
        tools: &[],
    };
    let cold = provider.complete(&request).expect("cold");
    let warm = provider.complete(&request).expect("warm");
    println!("cold={:?} warm={:?}", cold.usage, warm.usage);
    assert_eq!(cold.usage.cached_input, 0);
    assert!(warm.usage.cached_input > cold.usage.cached_input.min(1));
}
