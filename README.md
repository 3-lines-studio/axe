# Axe

A small, fast coding agent in one binary.

Axe includes:

- An OpenAI-compatible agent loop
- A terminal UI and one-shot mode
- Project-scoped sessions, resume, rewind, search, and compaction
- `read`, `write`, `edit`, and unsandboxed `bash`

It has no plugins, external tools, skills, web fetcher, sidecar, or TUI framework.

## Build

```sh
cargo +nightly build --release --config 'build.rustflags="-C force-unwind-tables=no"'
```

Nightly Rust and the `rust-src` component are required. Axe loads system libcurl at runtime.

## Use

```sh
export OPENAI_API_KEY="..."
axe
axe -C /path/to/project
axe "fix the failing tests"
axe --resume last
```

Use another OpenAI-compatible endpoint:

```sh
axe --base http://localhost:11434/v1 --model qwen3
```

The TUI supports streamed Markdown, tool status, file completion, session resume, rewind, search, compaction, model selection, and login. Run `/help` for its commands.

Axe stores config and project-scoped sessions under `~/.config/axe` or `$XDG_CONFIG_HOME/axe`.

## Test

```sh
make check
```

This runs formatting, clippy, unit tests, integration tests, a release build, and the black-box PTY harness.

Run live model evals separately:

```sh
OPENAI_API_KEY="..." make eval
AXE_EVAL_MODEL=gpt-4.1 OPENAI_API_KEY="..." make eval
python3 scripts/eval.py --bin target/release/axe --runs 3
```

Evals use temporary projects and check filesystem state and test results. They cost money and can vary by model, so `make check` does not run them.

## Bash

Bash runs directly on the host in the selected work directory. Axe captures stdout and stderr, reports exit status, truncates large output while preserving the full output in a temporary file, supports timeouts, and kills the command process group on timeout or cancellation.

Axe does not ask for tool permission and does not provide a sandbox.
