.PHONY: all build build-macos check ci clean docs-check docs-cli fmt fmt-check lint lint-md lint-rust test

# Default target
all: check

build:
	cargo build --locked

# Cross-compiles the workspace for macOS from Linux to verify build correctness, without running
# macOS tests. Needs a real macOS SDK (SDKROOT) because zaz-daemon depends on notify-rust, whose
# macOS backend compiles Objective-C against Cocoa/Foundation headers in its build.rs; zig alone
# doesn't bundle those. See the setup comment in cloudbuild.yaml for how to vendor one.
build-macos:
	@test -n "$(SDKROOT)" || (echo "SDKROOT must point to a macOS SDK; see the setup comment in cloudbuild.yaml" >&2 && exit 1)
	rustup target add aarch64-apple-darwin
	cargo install --locked cargo-zigbuild@0.23.0
	CARGO_ZIGBUILD_ZIG_PATH=$(CURDIR)/bin/zig cargo zigbuild --target aarch64-apple-darwin --locked

release:
	cargo build --release

install:
	cargo install --path .

test:
	cargo test --workspace --locked

lint: lint-rust lint-md

lint-rust:
	cargo clippy --all-targets --all-features --locked -- -D warnings

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
	cargo run --quiet --locked -p xtask -- docs-cli

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
