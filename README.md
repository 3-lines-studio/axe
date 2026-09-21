# Axe

A small, fast coding agent in one binary.

Axe includes:

- An OpenAI-compatible agent loop
- A terminal UI and one-shot mode
- Project-scoped sessions, resume, rewind, search, and compaction
- `read`, `write`, `edit`, and unsandboxed `bash`
- `search` and `fetch`: DuckDuckGo and a page-to-Markdown reader

It has no plugins, external tools, skills, or sidecar.

## Install

Download the latest release for Linux or macOS:

```sh
curl -fsSL https://github.com/3-lines-studio/axe/releases/latest/download/install.sh | sh
```

This installs `axe` to `~/.local/bin`. Set `AXE_PREFIX` to install elsewhere. Releases support Linux on x86_64 and aarch64, and macOS on Apple silicon.

Build from source with nightly Rust and the `rust-src` component:

```sh
cargo +nightly build --release --config 'build.rustflags=["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always"]'
```

Axe loads system libcurl at runtime.

## Use

```sh
export OPENAI_API_KEY="..."
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

The TUI supports streamed Markdown, tool status, file completion, session resume, rewind, and compaction. Run `/help` in the TUI for its commands. Use `axe --resume` or `axe -r` to open the session picker. One-shot mode requires `--resume last` or `--resume ID`.

## Config

Axe reads `~/.config/axe/config`, or `$XDG_CONFIG_HOME/axe/config` when `XDG_CONFIG_HOME` is set:

```ini
api_key = "..."
model = "gpt-4.1-mini"
base = "https://api.openai.com/v1"
context_window = 128000
```

`OPENAI_API_KEY` overrides `api_key`. The command-line `--model` and `--base` flags override the file. Set `context_window` to enable automatic compaction before the configured limit. Extra system instructions go in `SYSTEM.md` beside the config file; `--system` replaces the full built-in system prompt for that run.

Sessions are scoped by project and stored under the same Axe config directory. Starting a fresh TUI archives the prior live session for that project.

## Test

```sh
make check
```

This runs formatting, clippy, unit tests, integration tests, a release build, and the black-box PTY harness.

Run the deterministic local performance benchmark:

```sh
make bench
python3 scripts/bench.py /path/to/baseline target/release/axe --runs 100
```

Run live model evals separately:

```sh
OPENAI_API_KEY="..." make eval
python3 scripts/eval.py --model gpt-4.1 --runs 3
python3 scripts/eval.py --bin target/release/axe --runs 3
python3 scripts/eval.py --agent axe --runs 3
python3 scripts/eval.py --agent pi --runs 3
OPENAI_API_KEY="..." make eval-compaction
AXE_EVAL_MODEL=gpt-4.1-mini AXE_EVAL_COMPACTION_CYCLES=10 OPENAI_API_KEY="..." make eval-compaction
```

The agent eval loads the endpoint, model, and API key from `~/.config/axe/config` by default and compares Axe with `pi`. Use `--agent` to run one agent. The compaction eval checks required-fact recall and context size after every compaction cycle. Evals cost money and can vary by model, so `make check` does not run them.

## Web

Two tools, both in process over the system libcurl:

- `search QUERY` hits DuckDuckGo's HTML view and returns a numbered list of titles, URLs, and snippets.
- `fetch URL` downloads the page, runs it through readability extraction, and returns the article as Markdown. The chrome, the navigation, and the script tags do not reach the model.

When the extracted text comes out nearly empty, the page probably builds itself with JavaScript, so Axe looks for `chromium`, `chromium-browser`, `google-chrome`, or `google-chrome-stable` on `PATH` and asks for the DOM after the scripts ran. If there is no browser installed, it returns what it read. Nothing is required: rendering is an upgrade, not a dependency.

## Bash
Bash runs directly on the host in the selected work directory. Axe captures stdout and stderr, reports exit status, truncates large output while preserving the full output in a temporary file, supports timeouts, and kills the command process group on timeout or cancellation.

Axe does not ask for tool permission and does not provide a sandbox.
