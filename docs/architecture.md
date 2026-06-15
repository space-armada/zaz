# zaz Architecture Overview

A modern file-watching task runner and process manager, written in Rust. This
document captures the design rationale and high-level architecture.

The configuration schema reference lives in [`configuration.md`](configuration.md).

## Design Rationale

zaz addresses common pain points in file-watching development tools:

**Configuration**: Many tools use custom DSLs that are hard to learn and tooling
can't parse. zaz uses standard TOML/JSON formats that editors understand and
automated tools can generate/modify.

**Process Management**: Development servers often leave orphan processes when the
watcher exits. zaz uses process groups to ensure all child processes are properly
cleaned up on exit.

**Integration**: Most watchers are black boxes with no way for external tools
(like coding agents or CI systems) to query their state. zaz exposes a Unix socket
API for process status, log access, and control commands.

**Output Organization**: When running multiple processes, their output typically
interleaves into an unreadable mess. zaz separates log streams per-process and
provides filtering/search capabilities.

**Interactivity**: Traditional watchers require restarting to trigger a rebuild.
zaz provides a TUI with keyboard shortcuts for common actions like restart,
filtering logs, and navigating between processes.

**Stdin Support**: Some tools (like `tailwind --watch`) fail when stdin isn't
available. zaz allocates PTYs by default so these tools work correctly.

**Dependencies**: Complex projects may need one watch group to complete before
another starts (e.g., build JS before starting Go server that serves it). zaz
supports explicit `depends_on` declarations between groups.

## Design Decisions

| Decision | Choice |
|----------|--------|
| **Config format** | Both TOML and JSON (auto-detect by extension) |
| **Async runtime** | Tokio |
| **TUI library** | Ratatui |
| **Variable syntax** | `${var}` style with `zaz:` namespace for built-ins |
| **Built-in variables** | `${zaz:files}`, `${zaz:dirs}`, `${zaz:root}`, `${zaz:prefix}` |
| **Stdin handling** | PTY allocation by default (`no_pty = true` to disable) |
| **Dependencies** | Named groups with `depends_on = ["group_name"]` |
| **Config validation** | `deny_unknown_fields` to catch typos |
| **Command naming** | "task" for run-to-completion, "daemon" for long-running |

## Component Architecture

A single daemon serves exactly one config. The CLI reaches it over a Unix socket
regardless of mode:

```text
┌─────────────────────────────────────────────────────────────────┐
│                         zaz CLI                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐  │
│  │   TUI Mode   │  │  Daemon Mode │  │  One-shot Mode (-p)  │  │
│  └──────┬───────┘  └──────┬───────┘  └──────────┬───────────┘  │
└─────────┼─────────────────┼─────────────────────┼───────────────┘
          │                 │                     │
          │    Unix Socket  │                     │
          ▼                 ▼                     ▼
┌─────────────────────────────────────────────────────────────────┐
│                        zaz-daemon                                │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐ │
│  │   Config    │  │    File     │  │    Process Manager      │ │
│  │   Parser    │  │   Watcher   │  │  ┌─────┐ ┌─────┐       │ │
│  │ (TOML/JSON) │  │  (notify)   │  │  │Prep │ │Daemon│ ...   │ │
│  └─────────────┘  └─────────────┘  │  └─────┘ └─────┘       │ │
│                                     └─────────────────────────┘ │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐ │
│  │  Variable   │  │    Log      │  │      API Server         │ │
│  │   System    │  │   Manager   │  │    (Unix Socket)        │ │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

To cover several configs in one session, a workspace supervisor spawns one
ordinary single-config daemon per member and routes project-qualified names to
them, leaving the single-config path byte-for-byte unchanged.

### Key Components

1. **Config Parser** - Loads TOML/JSON, validates, provides typed config
2. **File Watcher** - Monitors filesystem, batches events, filters by patterns
3. **Variable System** - Expands `${zaz:files}`, `${zaz:dirs}`, custom variables
4. **Process Manager** - Runs tasks/daemons, handles signals, manages PTYs
5. **Log Manager** - Captures output per-process, supports filtering
6. **API Server** - Unix socket for IPC, allows external tooling integration
7. **TUI** - Ratatui-based interface with keyboard shortcuts

## Config Schema Evolution Policy

zaz follows **Cargo's approach** to config compatibility:

1. **Strict validation**: Unknown fields are rejected (`deny_unknown_fields`)
   - Catches typos immediately
   - Clear error messages

2. **No version field**: Config schema evolves carefully without explicit versioning
   - New fields must be optional with sensible defaults
   - Existing field semantics cannot change
   - Breaking changes require a new major version of zaz itself

3. **Backwards compatibility commitment**:
   - Old configs should work with new zaz versions
   - New optional features don't break existing configs

**Why not a version field?**

Many tools (Docker Compose, Kubernetes) use version fields, but they serve
different purposes:

| Approach | Unknown fields | Version field | Trade-off |
|----------|---------------|---------------|-----------|
| Docker Compose | Ignored | Required | Typos silently ignored |
| Kubernetes | Rejected | Required | Old tools can't read new configs |
| Cargo/ESLint | Rejected | None | Maintainer discipline required |
| zaz | Rejected | None | Same as Cargo |

Since zaz uses `deny_unknown_fields`, a version field would only help if we
needed breaking changes. Instead, we commit to backwards-compatible evolution.

**If breaking changes become necessary**, add a version field at that time:

```toml
version = 2  # Only added when v2 schema is introduced
```

This defers complexity until it's actually needed.
