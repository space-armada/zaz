.PHONY: all build check ci clean docs-check docs-cli fmt fmt-check lint lint-md lint-rust test

# Default target
all: check

build:
	cargo build

release:
	cargo build --release

install:
	cargo install --path .

test:
	cargo test --workspace

lint: lint-rust lint-md

lint-rust:
	cargo clippy --all-targets --all-features -- -D warnings

lint-md:
	bin/rumdl check .

fmt:
	cargo fmt
	bin/rumdl check --fix .

fmt-check:
	cargo fmt --check
	bin/rumdl check .

ci: fmt-check lint build test docs-check

docs-cli:
	cargo run --quiet -p xtask -- docs-cli --write

docs-check:
	cargo run --quiet -p xtask -- docs-cli

clean:
	cargo clean

watch:
	cargo watch -x check -x test

deps:
	rustup component add clippy rustfmt

run-debug-daemon: build
	./target/debug/zaz --debug daemon

run-debug-tui: build
	./target/debug/zaz
