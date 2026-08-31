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
cargo +nightly build --release --config 'build.rustflags=["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always"]'
```

Nightly Rust and the `rust-src` component are required. Axe loads system libcurl at runtime.

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

The TUI supports streamed Markdown, tool status, file completion, session resume, rewind, and compaction. Run `/help` for its commands. One-shot mode resumes a named session with `--resume last` or `--resume ID` and continues after compaction when `context_window` is set.

Axe stores config and project-scoped sessions under `~/.config/axe` or `$XDG_CONFIG_HOME/axe`. Extra system instructions go in `SYSTEM.md` there.

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
AXE_EVAL_MODEL=gpt-4.1 OPENAI_API_KEY="..." make eval
python3 scripts/eval.py --bin target/release/axe --runs 3
python3 scripts/eval.py --agent axe --runs 3
python3 scripts/eval.py --agent pi --runs 3
OPENAI_API_KEY="..." make eval-compaction
AXE_EVAL_MODEL=gpt-4.1-mini AXE_EVAL_COMPACTION_CYCLES=10 OPENAI_API_KEY="..." make eval-compaction
```

The agent eval loads the endpoint, model, and API key from `~/.config/axe/config` by default and compares Axe with `pi`. Use `--agent` to run one agent. The compaction eval checks required-fact recall and context size after every compaction cycle. Evals cost money and can vary by model, so `make check` does not run them.

## Bash

Bash runs directly on the host in the selected work directory. Axe captures stdout and stderr, reports exit status, truncates large output while preserving the full output in a temporary file, supports timeouts, and kills the command process group on timeout or cancellation.

Axe does not ask for tool permission and does not provide a sandbox.
