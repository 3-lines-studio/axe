# Axe

Minimal LLM coding agent harness, as a library.

The loop is the whole thing: messages → LLM → tool calls → results → repeat. It
lives in `run` and never mutates its input. Axe ships the loop, an
OpenAI-compatible provider, the tools, project-scoped sessions, compaction, and
the system prompt for a tool set. It ships no UI and no binary: the embedder
drives it and renders it.

## Embed

```toml
[dependencies]
axe = { git = "https://github.com/3-lines-studio/axe", rev = "<commit>" }
```

```rust
use axe::run::{self, RunOptions, Sink};
use axe::{Message, OpenAI};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

struct Silent;
impl Sink for Silent {}

let tools = axe::tools::build_tools("/tmp/project");
let system = axe::system_prompt(&tools);
let provider = OpenAI::new("https://api.openai.com/v1".into(), api_key);
let history = vec![Message {
    role: "user".into(),
    content: "list the files".into(),
    ..Default::default()
}];
let cancel = Arc::new(AtomicBool::new(false));
let opts = RunOptions {
    model: "gpt-4.1-mini",
    system: &system,
    tools: &tools,
    max_turns: usize::MAX,
};
let mut sink = Silent;
let end = run::run_stream(&provider, &opts, &history, &cancel, &mut sink);
```

`RunEnd::messages` is the grown transcript and `RunEnd::outcome` says why the
run stopped (`Done`, `MaxTurns`, `Cancelled`, `Compact`, `Failed`). On
`Failed`, a context overflow is retryable after compaction; `session` has the
helpers for that path.

The pieces:

- `run` — the loop, the `Sink` an embedder implements to observe it, and the
  `Outcome`.
- `openai` — the provider, with streaming and cancellation.
- `tools` — `read`, `write`, `edit`, `bash`, `search`, and `fetch`;
  `Tool` / `new_tool` / `new_tool_with_progress` to add your own.
- `session` — `Entry` records, `Store` / `FsStore`, compaction, and the
  project-scoped session directory.
- `machine` — the volume and the shell behind the tools, so a remote machine
  can take the place of the local one.
- `system_prompt(tools)` — Axe's tool list; embedders compose identity and
  guidelines around it.
- `image`, `sentinel`, `atomic_write` — attachments, secret redaction, and
  crash-safe writes.

## Config

`system_prompt` is the tool list only. Extra instructions for an embedder that
wants them live in `~/.config/axe/SYSTEM.md`, or `$XDG_CONFIG_HOME` when set;
`user_system_prompt()` returns them.

Sessions are scoped by project and stored under the same Axe config directory.

## Web

Two tools, both in process over the system libcurl:

- `search QUERY` hits DuckDuckGo's HTML view and returns a numbered list of
  titles, URLs, and snippets.
- `fetch URL` downloads the page, runs it through readability extraction, and
  returns the article as Markdown. The chrome, the navigation, and the script
  tags do not reach the model.

When the extracted text comes out nearly empty, the page probably builds
itself with JavaScript, so Axe looks for `chromium`, `chromium-browser`,
`google-chrome`, or `google-chrome-stable` on `PATH` and asks for the DOM after
the scripts ran. If there is no browser installed, it returns what it read.
Nothing is required: rendering is an upgrade, not a dependency.

## Bash

Bash runs directly on the host in the selected work directory. Axe captures
stdout and stderr, reports exit status, truncates large output while preserving
the full output in a temporary file, supports timeouts, and kills the command
process group on timeout or cancellation.

Axe does not ask for tool permission and does not provide a sandbox.

## Test

```sh
make check
```

This runs formatting, clippy with warnings as errors, and `cargo test`. The
`live-*` integration tests are `#[ignore]`d: they need `OPENAI_API_KEY` and
the network.

Axe loads system libcurl at runtime.

Build with nightly Rust and the `rust-src` component.
