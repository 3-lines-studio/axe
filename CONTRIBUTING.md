# Contributing

## Quality gate

Run `make check` before finishing any change. It runs fmt, clippy with warnings
as errors, and `cargo test`:

```bash
make check
```

Cheaper daily loop (autofixes without the gate):

```bash
cargo clippy --fix --allow-dirty && cargo fmt && cargo test
```

## Known exceptions

- `src/curlffi.rs` dlopens libcurl at runtime. The dlsym→fn-pointer
  transmutes are deliberate and annotated with explicit types. Do not
  "simplify" them.

## Project rules

- Keep dependencies minimal. No compile-time curl.
- Extra instructions live in `~/.config/axe/SYSTEM.md`. No plugin system,
  no MCP, no permissions layer.
- Config and project-scoped sessions live under `~/.config/axe/`.
- Axe is a library: no UI, no binary, no terminal handling.
