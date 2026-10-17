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

Two tools, both in process over the system libcurl, backed by DeepSeek's
Search and Fetch APIs:

- `search QUERY` posts to `/v1/tools/search` and returns a numbered list of
  results with title, URL, the long summary meant for a model, and the site
  and date when the API knows them. It takes `count` (1 to 50, 8 by default);
  the query supports `site:`, `-site:`, `"exact phrase"`, `-term`, `lang:`,
  `after:` and `before:`.
- `fetch URL` posts to `/v1/tools/fetch` and returns the page title and its
  content as Markdown. The chrome, the navigation and the scripts never reach
  the model.

Both need a key: `DEEPSEEK_API_KEY`, plus `DEEPSEEK_BASE_URL` to point at a
gateway. `web::Web` takes the same pair explicitly. Without one,
`build_tools` leaves the two out and the model is offered only the machine
tools. The provider bills $0.75 per 1,000 successful requests and limits each
API to 5 requests per second per account; the tools retry 408, 429 and 5xx
twice before giving up.

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
