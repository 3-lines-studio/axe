RELEASE = cargo +nightly build --release --config 'build.rustflags=["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always"]'
PREFIX ?= $(HOME)/.local

.PHONY: check run dev harness bench eval eval-compaction install

check:
	cargo fmt
	cargo clippy --all-targets -- -D warnings
	cargo test
	$(RELEASE)
	python3 scripts/harness.py --bin target/release/axe

run:
	$(RELEASE)
	./target/release/axe

dev:
	cargo build
	./target/debug/axe

harness:
	$(RELEASE)
	python3 scripts/harness.py --bin target/release/axe

bench:
	$(RELEASE)
	python3 scripts/bench.py target/release/axe --runs 100

eval:
	$(RELEASE)
	python3 scripts/eval.py --bin target/release/axe

eval-compaction:
	cargo test --test live-compaction -- --ignored --nocapture

install:
	$(RELEASE)
	install -d "$(PREFIX)/bin"
	install -m 0755 target/release/axe "$(PREFIX)/bin/axe"
