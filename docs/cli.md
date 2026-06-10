# CLI reference

Reference for every `zaz` subcommand and its flags. The TUI keyboard reference
lives in [tui.md](tui.md). The MCP tool server is documented in
[mcp.md](mcp.md). For the project config and user config schemas, see
[configuration.md](configuration.md) and
[user-configuration.md](user-configuration.md).

This file is the canonical operator reference. Shell completions
(`zaz completions <shell>`) and man pages (`zaz man [COMMAND]`) derive from
the same `clap` definition; see [Generated surfaces](#generated-surfaces)
for the sync model and the error-message contract.

## Overview

`zaz` is the single binary for every workflow: it can launch a TUI, run the
daemon directly in the foreground, start it in the background, query or
manage a running daemon, validate config, and serve as an MCP tool over
stdio. With no subcommand, `zaz` opens the TUI and autostarts a daemon when
one is not already running.

The subcommands fall into three exit-policy categories:

- **Query** — `status`. Reports state; `0` means running, `3` means not
  running, `1` means an operational error.
- **Strict-mutating** — `restart`, `reload`. Require a running daemon; `1`
  on absence or API failure.
- **Idempotent-mutating** — `start`, `stop`. Ensure a postcondition; `0`
  even when the daemon is already in the desired state.

The categories are spelled out in `src/main.rs` above the `Commands` enum.

## Generated surfaces

The single source of truth for every operator-facing CLI artifact is the
`clap` command tree exported by `Cli::command()` in `src/cli.rs`. Three
distinct surfaces derive from it; each is regenerated, never hand-edited:

- **This reference's flag and argument tables.** The `<!-- BEGIN: zaz <name> -->` /
  `<!-- END: zaz <name> -->` blocks inside every subcommand section below are
  emitted by the `xtask` walker at `crates/xtask/src/docs_cli.rs`. Run
  `make docs-cli` to regenerate; `make docs-check` runs the same walker in
  drift-detection mode and is wired into `make ci`. Hand-written prose
  outside the markers is preserved across regenerations.
- **Shell completions.** `zaz completions <shell>` writes a completion
  script to stdout via `clap_complete::generate`. The supported shells are
  whatever `clap_complete::Shell` exposes — bash, zsh, fish, elvish, and
  powershell — without a wrapper enum, so new shells added upstream become
  available without code changes here.
- **Man pages.** `zaz man [COMMAND]` writes a roff document to stdout via
  `clap_mangen::Man`. With no argument the page covers the root `zaz`
  binary; passing a subcommand name renders that subcommand's page with the
  conventional `ZAZ-<NAME>(1)` header. Unknown subcommand names exit
  non-zero with `unknown subcommand: <name>` rather than silently rendering
  the root page.

Richer per-subcommand prose lives in this file rather than in clap's
`long_about` / `after_help` attributes, which are intentionally unset.
Keeping prose in one location avoids the drift risk of two parallel content
streams; completions and man pages stay accurate because they derive from
the structural attributes (flag names, types, short help) that are already
the source of every table in this file.

### Error-message contract

Operator-facing error wording follows a fixed shape. The shape exists so
scripted consumers can parse `zaz` output without regexing free-form prose,
and so operators always know where the recovery suggestion lives.

- **Recovery hints are structured.** Error types whose recovery prose is
  fixed per variant — `DaemonError` in `crates/zaz-daemon/src/error.rs` and
  `McpError` in `crates/zaz-mcp/src/error.rs` — expose a
  `pub fn hint(&self) -> Option<&'static str>` accessor. `ValidationError`
  in `crates/zaz-config/src/error.rs` carries an instance-level
  `hint: Option<String>` field instead, because the disambiguation text
  depends on construction-site context.
- **Hints render on a separate line.** The binary's `report_error`
  printer in `src/main.rs` walks the `anyhow` error chain, finds the first
  variant that exposes a hint, and emits two lines: an `Error: <message>`
  line followed by an indented `hint: <recovery>` line. ANSI coloring is
  gated on `std::io::stderr().is_terminal()`, so non-TTY consumers see
  plain text and substring matchers stay stable. The same shape powers
  `zaz check`'s validation pretty-printer.
- **Daemon-API verbs share wording.** The four verbs that talk to a
  running daemon (`status`, `restart`, `stop`, `reload`) route through
  two helpers in `src/main.rs`: `connect_or_no_daemon` and
  `handle_daemon_response`. The wording is uniform across all four:
  `no daemon running at <socket>` (with a hint pointing at `zaz start`
  and `--socket <PATH>`), `<verb> failed: <message>` on an API error, and
  `<verb> returned unexpected response` on the catch-all branch. The
  inner `Error:` prefix that earlier versions of zaz emitted is gone;
  the top-level `report_error` printer is the only source of that
  prefix.

Adding a new error variant with a recovery action means adding it to
`hint()` (or, for context-bearing variants, attaching a `hint` at the
construction site); adding a new daemon-talking verb means routing through
the two helpers above so the wording stays in lockstep.

## Global flags

The flags below apply to every subcommand:

- `-c`, `--config <CONFIG>` — path to the project config file. Overrides
  upward discovery (see [Socket and config resolution](#socket-and-config-resolution)).
- `-s`, `--socket <SOCKET>` — explicit daemon socket path. Takes precedence
  over the socket derived from `--config`.
- `-d`, `--debug` — verbose logging. In TUI mode this also enables
  per-process debug log files; see [Log files and rotation](#log-files-and-rotation).
- `--log-file <PATH>` — write debug logs to the given file. Works in both
  TUI and daemon modes.

The TUI-only flags `--full`, `--multi-pane`, `--no-autostart`, and
`--stop-on-exit` apply to the default `zaz` invocation and are documented
under [zaz (default, TUI mode)](#zaz-default-tui-mode).

## Socket and config resolution

Every subcommand resolves its target socket the same way:

1. If `--socket <PATH>` is passed, use it verbatim. Explicit always wins.
2. Otherwise, discover the project config by walking upward from CWD until
   `zaz.toml` or `zaz.json` is found (or the path supplied to `--config` is
   used directly). The socket is derived deterministically from the
   canonical config path via a hash, so two different project directories
   never share a socket by accident.
3. If no config is found and no `--socket` is given, the command errors with
   an actionable message rather than falling back to a global default.

`docs/mcp.md` defers to this section; the `--socket` and `--config` flags
behave the same way for `zaz mcp`.

## Exit codes

zaz follows the LSB / `systemctl` convention:

- `0` — success or idempotent no-op.
- `1` — operational error (failed to start, API call failed, validation
  failed, I/O error, etc.).
- `3` — "not running" (queried daemon is absent).

Mapping to subcommands:

| Subcommand | `0` | `1` | `3` |
|------------|-----|-----|-----|
| `status` | running | operational error | not running |
| `restart` | restart succeeded | no daemon, or API error | — |
| `reload` | reload succeeded | no daemon, or API error | — |
| `start` | running (already or just started) | start failed | — |
| `stop` | stopped (already or just stopped) | stop failed | — |
| `task` | all tasks finished cleanly | any task failed | — |
| `check` | config is valid | config invalid or unreadable | — |

## Log files and rotation

zaz writes daemon log files under `$XDG_STATE_HOME/zaz/` when set, otherwise
`~/.local/state/zaz/`. The default file set:

- `daemon-output.log` — always written for daemons launched via TUI
  autostart or `zaz start`; captures panics and pre-init errors.
- `tui-debug.log` and `daemon-debug.log` — written when `--debug` is passed
  in TUI mode.
- A user-provided `--log-file <PATH>` overrides the TUI debug file; the
  autostarted daemon uses a sibling `*.daemon.log` file derived from that
  path.

Rotation parameters:

| Parameter | Value |
|-----------|-------|
| Per-file size cap | 10 MB |
| Generations kept | 5 |
| Total budget across rotations | 200 MB |

Oldest rotated files are pruned first when the budget is exceeded.

## API log persistence

The structured per-process log stream that `zaz_logs`, the TUI, and the
daemon API read back is a separate surface from the debug log files
described above. By default it lives in a bounded in-memory buffer that
is lost when the daemon exits. With `backend = "sqlite"` in user config
the same stream is also written to
`$XDG_STATE_HOME/zaz/logs/<config-hash>.sqlite3` (falling back to
`~/.local/state/zaz/logs/`), so historical queries return rows written
before the most recent restart. The query shape and exit codes are
unchanged across modes.

See [user-configuration.md#log_storage](user-configuration.md#log_storage)
for backend selection, retention limits, the database location
convention, and the degraded-mode contract.

Two behaviors are worth flagging for scripted consumers:

- **Offset pagination under concurrent retention.** Pagination uses
  `offset` and `limit`. When the daemon prunes rows mid-read — which
  happens after every batch write and on a bounded periodic cadence —
  the page after the one you just received may have shifted underneath
  you. A cursor-based pagination contract is a future proposal; the
  current MCP and CLI request shape stays offset-based.
- **Group rename semantics.** `group_name` on each row is a write-time
  snapshot, so renaming a group in `zaz.toml` does not retroactively
  retag historical rows. Filter by process name or by the original
  group name to reach logs written before the rename.

## Subcommands

### zaz (default, TUI mode)

Launches the interactive TUI. Autostarts a background daemon when one is not
already running, unless `--no-autostart` is passed. Connects to the daemon
over the resolved socket.

The TUI is documented in [tui.md](tui.md), including styles, keyboard
shortcuts, and filter/search semantics. `--full` and `--multi-pane` choose a
style at launch and override any user-config preference.

Examples:

```sh
zaz                    # open TUI in user-preferred style; autostart daemon
zaz --full             # force the Full style for this run
zaz --multi-pane       # force the Multi-Pane style
zaz --no-autostart     # require an existing daemon; fail if absent
zaz --stop-on-exit     # stop the daemon when the TUI exits
```

<!-- BEGIN: zaz -->
**Flags**

| Flag | Default | Description |
|------|---------|-------------|
| `-c`, `--config` `<CONFIG>` | — | Configuration file path (repeatable; 2+ starts a workspace) |
| `-d`, `--debug` | `false` | Enable debug logging |
| `-s`, `--socket` `<SOCKET>` | — | Socket path for daemon communication |
| `--full` | `false` | Use full TUI style (split panes with group tree) |
| `--multi-pane` | `false` | Use multi-pane TUI style (one pane per task) |
| `--no-autostart` | `false` | Don't auto-start a daemon before opening the TUI |
| `--stop-on-exit` | `false` | Stop the connected daemon when the TUI exits |
| `--log-file` `<PATH>` | — | Write debug logs to a file (works in both TUI and daemon modes) |

<!-- END: zaz -->

### zaz task

Runs every group's tasks once, sequentially with fail-fast semantics, and
exits. Tasks are run-to-completion units (as opposed to long-running
daemons). Useful for one-shot builds, lints, or test runs from a script
without the TUI or a persistent daemon.

Exits `0` only if every task in every group exits cleanly; `1` if any task
fails.

<!-- BEGIN: zaz task -->
This subcommand takes no arguments or flags.

<!-- END: zaz task -->

### zaz daemon

Runs the daemon in the foreground. Exits when the user interrupts it
(Ctrl+C) or the daemon is asked to stop over its socket. This mode never
daemonizes; use `zaz start` to launch a background daemon.

`--quiet` suppresses per-process stdout/stderr from the daemon's own log
output; per-process logs are still recorded for the TUI and `zaz status` to
read back.

<!-- BEGIN: zaz daemon -->
**Flags**

| Flag | Default | Description |
|------|---------|-------------|
| `-q`, `--quiet` | `false` | Suppress process output logging |

<!-- END: zaz daemon -->

### zaz start

Idempotent-mutating: ensures a daemon is running for the resolved config and
exits. If a daemon is already running on the resolved socket, exits `0`
without doing anything. The newly started daemon writes
`daemon-output.log`; see [Log files and rotation](#log-files-and-rotation).

<!-- BEGIN: zaz start -->
This subcommand takes no arguments or flags.

<!-- END: zaz start -->

### zaz stop

Idempotent-mutating: ensures no daemon is running for the resolved socket.
Exits `0` whether the daemon was running and was stopped, or was already
absent. Operational errors (socket I/O failure, API error during shutdown)
still exit `1`.

<!-- BEGIN: zaz stop -->
This subcommand takes no arguments or flags.

<!-- END: zaz stop -->

### zaz status

Query: reports whether a daemon is running on the resolved socket and a
short summary of running groups when it is.

| Exit | Meaning |
|------|---------|
| `0` | daemon running |
| `1` | operational error (e.g. socket unreadable) |
| `3` | daemon not running |

The `3` exit is the LSB / `systemctl` "not running" convention; scripts can
distinguish "absent" from "broken" without parsing stderr.

<!-- BEGIN: zaz status -->
This subcommand takes no arguments or flags.

<!-- END: zaz status -->

### zaz restart

Strict-mutating: tells a running daemon to restart a single group, or every
group when no name is given. The daemon executes the same startup hook
chain it ran at boot, so `restart` is the supported way to
re-run prep tasks without bouncing the daemon itself.

Exits `1` when no daemon is running on the resolved socket, when the named
group does not exist, or when the daemon API returns an error.

<!-- BEGIN: zaz restart -->
**Positional arguments**

| Argument | Required | Description |
|----------|----------|-------------|
| `GROUP` | no | Group name to restart (omit for all) |

<!-- END: zaz restart -->

### zaz reload

Strict-mutating: tells a running daemon to reread its config file, validate
it, and apply diffs. Reload preserves the daemon process and its socket;
running daemons stay up if their definitions did not change. Reload does
not re-run startup tasks — use `zaz restart` for that.

Exits `1` when no daemon is running on the resolved socket or when the new
config fails validation; the running daemon keeps its previous config in
that case.

<!-- BEGIN: zaz reload -->
This subcommand takes no arguments or flags.

<!-- END: zaz reload -->

### zaz check

Validates a project config file without starting a daemon. With no argument,
checks the result of [Socket and config resolution](#socket-and-config-resolution)
(typically `./zaz.toml` or `./zaz.json`). Exits `0` on success and `1` when
validation or parsing fails.

`--json` emits a single JSON object on stdout. Schema:

```json
{
  "valid": false,
  "path": "zaz.toml",
  "errors": [
    {
      "line": 12,
      "column": 3,
      "message": "group[1]: name cannot be empty",
      "hint": "set a non-empty name on this group",
      "code": "empty_group_name"
    }
  ]
}
```

Field semantics:

| Field | Type | Notes |
|-------|------|-------|
| `valid` | bool | `true` only if the config parses and validates. |
| `path` | string | The config path that was checked. |
| `errors` | array | Empty when `valid` is `true`. |
| `errors[].line` / `.column` | integer (optional) | Source location when known. |
| `errors[].message` | string | The same message printed in non-JSON mode. |
| `errors[].hint` | string (optional) | Suggested fix when one is available. |
| `errors[].code` | string | Stable machine-readable code from the table below. |

Validation error codes (from `crates/zaz-config/src/error.rs`):

| Code | Meaning |
|------|---------|
| `empty_group_name` | Group has no name. |
| `duplicate_group_name` | Two groups share a name. |
| `empty_group` | Group has no patterns and no commands. |
| `unknown_dependency` | `depends_on` references a missing group. |
| `self_dependency` | Group lists itself in `depends_on`. |
| `dependency_cycle` | Cycle detected across `depends_on` edges. |
| `invalid_pattern` | A `patterns` entry failed glob parsing. |
| `invalid_ignore_pattern` | An `ignore` entry failed glob parsing. |
| `empty_task_command` | Task has an empty `command`. |
| `duplicate_task_name` | Two tasks in the same group share an explicit name. |
| `empty_daemon_command` | Daemon has an empty `command`. |
| `duplicate_daemon_name` | Two daemons in the same group share an explicit name. |

Parse-level failures (TOML/JSON syntax errors, unknown fields, I/O errors)
are reported as a single error with code `parse_error`.

<!-- BEGIN: zaz check -->
**Positional arguments**

| Argument | Required | Description |
|----------|----------|-------------|
| `FILE` | no | Configuration file to check (defaults to zaz.toml or zaz.json) |

**Flags**

| Flag | Default | Description |
|------|---------|-------------|
| `--json` | `false` | Output as JSON for tooling integration |

<!-- END: zaz check -->

### zaz mcp

Runs the MCP tool server over stdio for use with Claude Code, Cursor, and
other MCP-aware clients. See [mcp.md](mcp.md) for the full tool list and
client configuration snippets. Socket and config resolution match the
[Socket and config resolution](#socket-and-config-resolution) section above.

`--autostart` spawns a background daemon at MCP startup if one is not
already running. Without this flag, MCP tools that require a running daemon
return an `McpError::DaemonRefused` to the client when the daemon is absent.

<!-- BEGIN: zaz mcp -->
**Flags**

| Flag | Default | Description |
|------|---------|-------------|
| `--autostart` | `false` | Spawn a background daemon at startup if one is not already running |

<!-- END: zaz mcp -->

### zaz ignores

Prints the compiled-in default ignore patterns, one per line. These patterns
are merged with each group's own `ignore` list when the watcher is set up;
they exist so that common SCM, editor, and build directories never trigger
rebuilds.

Current defaults:

- `**/.git/**`
- `**/.hg/**`
- `**/.svn/**`
- `**/.DS_Store`
- `**/node_modules/**`
- `**/target/**`
- `**/*.swp`
- `**/*~`
- `**/#*#`
- `**/.#*`

<!-- BEGIN: zaz ignores -->
This subcommand takes no arguments or flags.

<!-- END: zaz ignores -->

### zaz completions

Print a shell completion script to stdout for the requested shell. Pipe the
output into the appropriate completion location for your shell.

<!-- BEGIN: zaz completions -->
**Positional arguments**

| Argument | Required | Description |
|----------|----------|-------------|
| `SHELL` | yes | Shell to generate completions for |

<!-- END: zaz completions -->

### zaz man

Print a roff-formatted man page to stdout. Without arguments, the page covers
the root `zaz` command; pass a subcommand name to render that subcommand's
page (header `ZAZ-<NAME>(1)`). Pipe the output to a file or to `man -l -` to
preview.

<!-- BEGIN: zaz man -->
**Positional arguments**

| Argument | Required | Description |
|----------|----------|-------------|
| `COMMAND` | no | Subcommand to render (omit for the root `zaz` page) |

<!-- END: zaz man -->
