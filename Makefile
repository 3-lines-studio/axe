RELEASE = cargo +nightly build --release --config 'build.rustflags=["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always"]'
PREFIX ?= $(HOME)/.local

.PHONY: check run dev harness eval install

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

eval:
	$(RELEASE)
	python3 scripts/eval.py --bin target/release/axe

install:
	$(RELEASE)
	install -d "$(PREFIX)/bin"
	install -m 0755 target/release/axe "$(PREFIX)/bin/axe"
