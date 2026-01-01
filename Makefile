.PHONY: all build check clean fmt fmt-check lint lint-md lint-rust test

# Default target
all: check

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

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

ci: fmt-check lint build test

clean:
	cargo clean

watch:
	cargo watch -x check -x test

deps:
	rustup component add clippy rustfmt
