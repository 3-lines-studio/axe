# Axe

A small, fast coding agent in one binary, and a library you can embed.

Axe includes:

- An OpenAI-compatible agent loop
- A terminal UI and one-shot mode
- Project-scoped sessions, resume, rewind, and compaction
- `read`, `write`, `edit`, and unsandboxed `bash`
- `search` and `fetch` through DeepSeek's tools API
- Agent Skills, discovered on disk and loaded on demand
- A library surface for embedding the loop in another program

It has no plugins, external tools, or sidecar.

## Install

Download the latest release for Linux or macOS:

```sh
curl -fsSL https://github.com/3-lines-studio/axe/releases/latest/download/install.sh | sh
```

This installs `axe` to `~/.local/bin`. Set `AXE_PREFIX` to install elsewhere. Releases support Linux on x86_64 and aarch64, and macOS on Apple silicon.

Build from source with nightly Rust and the `rust-src` component:

```sh
make run
```

or directly:

```sh
cargo +nightly build --release --config 'build.rustflags=["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always"]'
```

Axe loads system libcurl at runtime.

## Use

```sh
export DEEPSEEK_API_KEY="..."
axe
axe -C /path/to/project
axe "fix the failing tests"
axe --resume last
axe --resume last "continue from there"
```

Use another OpenAI-compatible endpoint:

```sh
axe --base http://localhost:11434/v1 --model qwen3
```

Attach images to a prompt (DeepSeek vision models):

```sh
axe --image screenshot.png "what is wrong in this screenshot?"
axe -i https://example.com/chart.png "summarize this chart"
```

`--image` takes a local file path or an http(s) URL, repeats, and needs a prompt, so it does not open the TUI. JPEG, PNG, GIF, and WebP are supported. Local files are sent inline as base64 data URLs; DeepSeek resizes and caps each image at 1024 tokens server-side, so no client-side resizing is needed.

In the TUI, `/image PATH` attaches the same way and `/image clear` drops the attachments. Dropping a file onto the terminal attaches it too. Attachments show above the input and are sent with the next message.

The agent can pull an image into its own context: reading a JPEG, PNG, GIF, or WebP with the `read` tool returns the image as part of the tool result, so it can inspect a screenshot or chart it found or produced. Reading any other binary file returns an error instead of dumping bytes into the context.

Flags also accept a single dash. Run `axe --help` for the full list. If standard input is not a terminal and no prompt is given, Axe reads the prompt from standard input.

The TUI supports streamed Markdown, tool status, file completion, session resume, rewind, and compaction. It reads the terminal's background once at startup and picks a light or dark palette to match. Run `/help` in the TUI for its commands. Use `axe --resume` or `axe -r` to open the session picker. One-shot mode requires `--resume last` or `--resume ID`.

## Config

Axe reads `~/.config/axe/config`, or `$XDG_CONFIG_HOME/axe/config` when `XDG_CONFIG_HOME` is set:

```ini
api_key = "..."
model = "deepseek-flash"
base = "https://api.deepseek.com"
# context_window = 1000000
```

`DEEPSEEK_API_KEY` or `OPENAI_API_KEY` overrides `api_key`; the same key is used for the model and, when the base URL is DeepSeek, for the `search` and `fetch` tools. The command-line `--model` and `--base` flags override the file. Reasoning effort defaults to `max`. Set `context_window` to enable automatic compaction before the configured limit. Extra system instructions go in `SYSTEM.md` beside the config file; `--system` replaces the full built-in system prompt for that run.

Sessions are scoped by project and stored under the same Axe config directory. Starting a fresh TUI archives the prior live session for that project.

## Skills

Axe reads [Agent Skills](https://agentskills.io) from disk: any subdirectory with a `SKILL.md` file. It looks in, in increasing precedence:

- `~/.config/axe/skills` (or `$XDG_CONFIG_HOME/axe/skills`)
- `~/.agents/skills`
- `<project>/.agents/skills`
- `<project>/.axe/skills`

A project skill shadows a user skill with the same `name`. Skills without a `description` in their frontmatter are skipped.

Nothing is loaded into the prompt up front. The agent gets two tools: `list_skills` returns the name and description of every installed skill, and `load_skill NAME` returns one skill's `SKILL.md` body, its directory, and the files it bundles. The agent reads those bundled files with `read` only when the instructions ask for it. If no skills are installed, neither tool is added.

In the TUI, `/skill NAME` loads a skill without waiting for the model to pick it. Type `/skill ` and the installed names complete.

```sh
npx skills add anthropics/skills
```

## Use as a library

```toml
[dependencies]
axe = { git = "https://github.com/3-lines-studio/axe", rev = "<commit>" }
```

```rust
let tools = axe::tools::build_tools("/tmp/project");
let system = axe::system_prompt(&tools);
let provider = axe::OpenAI::new("https://api.deepseek.com", api_key).effort("max");
let end = axe::run::run_stream_with(
    &provider, &opts, &history, &cancel, &mut observer, &mut control,
);
```

The `tui` feature is on by default and can be turned off with
`default-features = false` for a headless library build.

## Test

```sh
make check
```

This runs formatting, clippy, unit and integration tests, a release build, and the black-box PTY harness.

Run the deterministic local performance benchmark:

```sh
make bench
python3 scripts/bench.py /path/to/baseline target/release/axe --runs 100
```

Run live model evals separately:

```sh
DEEPSEEK_API_KEY="..." make eval
python3 scripts/eval.py --model deepseek-flash --runs 3
python3 scripts/eval.py --bin target/release/axe --runs 3
python3 scripts/eval.py --agent axe --runs 3
python3 scripts/eval.py --agent pi --runs 3
DEEPSEEK_API_KEY="..." make eval-compaction
AXE_EVAL_MODEL=deepseek-flash AXE_EVAL_COMPACTION_CYCLES=10 DEEPSEEK_API_KEY="..." make eval-compaction
```

The agent eval loads the endpoint, model, and API key from `~/.config/axe/config` by default and compares Axe with `pi`. Use `--agent` to run one agent. The compaction eval checks required-fact recall and context size after every compaction cycle. Evals cost money and can vary by model, so `make check` does not run them.

## Web

Two tools over the system libcurl, backed by DeepSeek's tools API:

- `search QUERY` returns ranked results with titles, URLs, and snippets.
- `fetch URL` returns the page as Markdown, without the navigation, ads, and scripts.

Both use the same key as the model: `DEEPSEEK_API_KEY`, the axe config key, or `OPENAI_API_KEY`, as long as the base URL is DeepSeek. Without a key the tools are left out of the tool set. `DEEPSEEK_BASE_URL` overrides the web endpoint (default `https://api.deepseek.com`). Weak search results are labelled so the model can tell a thin hit from a good one.

## Bash

Bash runs directly on the host in the selected work directory. Axe captures stdout and stderr, reports exit status, truncates large output while preserving the full output in a temporary file, supports timeouts, and kills the command process group on timeout or cancellation.

Axe does not ask for tool permission and does not provide a sandbox.
