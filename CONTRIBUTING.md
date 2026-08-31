# Contributing

## Quality gate

Run `make check` before finishing any change. It runs fmt, clippy,
`cargo test`, the release build, and the black-box harness:

```bash
make check
```

The release build catches linker/panic-abort issues that unit tests miss.
`scripts/harness.py` (also runnable standalone via `make harness`) drives the
real binary end to end with a mock OpenAI endpoint and a PTY for the TUI; it
needs python3, already a dev dependency for `scripts/mock-server.py`.

Cheaper daily loop (autofixes without the gate):

```bash
cargo clippy --fix --allow-dirty && cargo fmt && cargo test
```

Install the current checkout to `~/.local/bin/axe` for local use:

```bash
make install
```

Set `PREFIX` to install elsewhere.

## Known exceptions

- `src/markdown.rs` is a byte-exact port of fx's markdown renderer. It has
  one targeted `#[allow(clippy::needless_update)]` on the `PROFILES` table:
  the `p!` macro legitimately fills partial field sets from `Profile::empty()`.
  Do not remove it or refactor the port's code beyond lint fixes.
- `src/curlffi.rs` dlopens libcurl at runtime. The dlsym→fn-pointer
  transmutes are deliberate and annotated with explicit types. Do not
  "simplify" them.

## Project rules

- Release binary stays under 1 MB (currently ~435 KB).
- Dependencies are exactly `serde`, `serde_json`, `libc`. No new deps
  without asking. No compile-time curl.
- Extra instructions live in `~/.config/axe/SYSTEM.md`. No plugin system,
  no MCP, no permissions layer.
- Config and project-scoped sessions live under `~/.config/axe/`.
