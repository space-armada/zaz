.PHONY: all bench build build-macos check ci clean dist-linux dist-macos docs-check docs-cli fmt fmt-check lint lint-md lint-rust test

# Default target
all: check

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-20s %s\n", $$1, $$2}'

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

# Directory where release binaries are staged for upload. Cloud Build checks the
# repo out at /workspace, so the release step overrides this to /workspace/dist.
DIST_DIR ?= dist

# Builds the optimized Linux binary and stages it under DIST_DIR with a triple-suffixed name.
dist-linux:
	cargo build --release --locked --bin zaz
	mkdir -p $(DIST_DIR)
	cp target/release/zaz $(DIST_DIR)/zaz-x86_64-unknown-linux-gnu

# Cross-compiles the optimized macOS binary via zigbuild and stages it. Needs SDKROOT, like
# build-macos; see that target and the setup comment in cloudbuild.yaml.
dist-macos:
	@test -n "$(SDKROOT)" || (echo "SDKROOT must point to a macOS SDK; see the setup comment in cloudbuild.yaml" >&2 && exit 1)
	rustup target add aarch64-apple-darwin
	cargo install --locked cargo-zigbuild@0.23.0
	CARGO_ZIGBUILD_ZIG_PATH=$(CURDIR)/bin/zig cargo zigbuild --release --target aarch64-apple-darwin --locked --bin zaz
	mkdir -p $(DIST_DIR)
	cp target/aarch64-apple-darwin/release/zaz $(DIST_DIR)/zaz-aarch64-apple-darwin

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

# Runs the Criterion benchmark harness. Deliberately kept out of the `ci` gate:
# benchmarks are non-gating and run on a manual or scheduled trigger only. See
# cloudbuild.bench.yaml for the policy and how results are tracked over time.
bench:
	cargo bench --workspace --locked

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
