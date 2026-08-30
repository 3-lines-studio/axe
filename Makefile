UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)
ifeq ($(UNAME_S),Linux)
TRIPLE := $(UNAME_M)-unknown-linux-gnu
NOPIE := ,"-Clink-args=-no-pie"
else ifeq ($(UNAME_S),Darwin)
TRIPLE := $(UNAME_M)-apple-darwin
endif
BIN := target/$(TRIPLE)/release/axe

RELEASE = cargo +nightly build --release --target $(TRIPLE) --config 'build.rustflags=["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always"$(NOPIE)]'
PREFIX ?= $(HOME)/.local

.PHONY: check run dev harness eval install

check:
	cargo fmt
	cargo clippy --all-targets -- -D warnings
	cargo test
	$(RELEASE)
	python3 scripts/harness.py --bin $(BIN)

run:
	$(RELEASE)
	./$(BIN)

dev:
	cargo build
	./target/debug/axe

harness:
	$(RELEASE)
	python3 scripts/harness.py --bin $(BIN)

eval:
	$(RELEASE)
	python3 scripts/eval.py --bin $(BIN)

install:
	$(RELEASE)
	install -d "$(PREFIX)/bin"
	install -m 0755 $(BIN) "$(PREFIX)/bin/axe"
