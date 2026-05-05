# zaz

zaz :: putting the zaz in pizzazz

A modern file-watching task runner and process manager for development
environments, heavily inspired by `modd`.

## Overview

zaz watches files, runs tasks, and supervises long-running daemons. A
background daemon owns process state and exposes a Unix socket API; the TUI,
one-shot CLI, and MCP tool server are all clients of that daemon.

## Install

From a clone of this repo:

```bash
cargo install --path .   # or: make install
```

Build without installing:

```bash
cargo build --release    # or: make release
```

The `Makefile` lists the common dev targets (`build`, `test`, `lint`, `fmt`,
`ci`, `watch`).

## Quick start

Create a `zaz.toml` in your project root:

```toml
[[group]]
name = "backend"
patterns = ["**/*.go"]

[[group.task]]
name = "build"
command = "go build -o ./bin/server ./cmd/server"

[[group.daemon]]
name = "server"
command = "./bin/server"
```

Run zaz:

```bash
zaz
```

Plain `zaz` opens the TUI. If a daemon is already running for the target
socket, the TUI reuses it; otherwise zaz autostarts one. Both `zaz.toml` and
`zaz.json` are accepted; see [docs/configuration.md](docs/configuration.md)
for the full schema.

## Core concepts

- **Groups** bind a set of file patterns to one or more commands. When a
  watched file changes, the group's tasks and daemons are re-run.
- **Tasks** run to completion. Use them for builds, tests, codegen, or any
  step that finishes.
- **Daemons** are long-running processes. zaz starts them on launch and
  signals them on change so they restart cleanly.
- **Patterns** are glob expressions over the working tree; a group can also
  list `ignore` patterns.
- **`depends_on`** orders groups so one group's tasks can complete before
  another starts (for example, build before serve).

## Lifecycle

The daemon is the source of truth for process state. Plain `zaz` autostarts a
daemon if needed and opens the TUI against it. `zaz start` launches a daemon
in the background; `zaz stop` and `zaz status` manage and inspect it. `zaz
daemon` runs the daemon in the foreground for direct supervision. `zaz task`
runs the configured tasks once and exits without starting a daemon. Full
command, flag, and exit-code reference lives in
[docs/cli.md](docs/cli.md).

## Documentation

- [docs/configuration.md](docs/configuration.md) — project config
  (`zaz.toml`/`zaz.json`) reference.
- [docs/user-configuration.md](docs/user-configuration.md) — per-user
  preferences and XDG paths.
- [docs/cli.md](docs/cli.md) — CLI command, flag, exit-code, and log-file
  reference.
- [docs/tui.md](docs/tui.md) — TUI styles, modes, and keyboard shortcuts.
- [docs/mcp.md](docs/mcp.md) — MCP tool server (`zaz mcp`) and client
  configuration.

## License

MIT
