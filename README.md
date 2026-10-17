# Axe

Minimal LLM coding agent harness, as a library.

```toml
[dependencies]
axe = { git = "https://github.com/3-lines-studio/axe", rev = "<commit>" }
```

```rust
let tools = axe::tools::build_tools("/tmp/project");
let system = axe::system_prompt(&tools);
let provider = OpenAI::new("https://api.openai.com/v1".into(), api_key);
let end = run::run_stream(&provider, &opts, &history, &cancel, &mut sink);
```

Nightly Rust and the `rust-src` component. `make check` runs fmt, clippy and the
tests; the `live-*` ones need a key and the network and are `#[ignore]`d.
