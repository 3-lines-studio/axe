.PHONY: check eval-compaction

check:
	cargo fmt
	cargo clippy --all-targets -- -D warnings
	cargo test

eval-compaction:
	cargo test --test live-compaction -- --ignored --nocapture
