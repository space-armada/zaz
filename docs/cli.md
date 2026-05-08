# CLI reference

Reference for every `zaz` subcommand and its flags. The TUI keyboard reference
lives in [tui.md](tui.md). The MCP tool server is documented in
[mcp.md](mcp.md). For the project config and user config schemas, see
[configuration.md](configuration.md) and
[user-configuration.md](user-configuration.md).

The flag tables inside each subcommand section are generated from the `clap`
command tree and live between `<!-- BEGIN: zaz <name> -->` and
`<!-- END: zaz <name> -->` markers. Hand-written prose around the markers is
preserved across regenerations. Run `make docs-cli` to update the tables;
`make docs-check` is wired into CI and fails on drift.

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

The categories are spelled out in `src/main.rs` above the `Commands` enum
and codified in [ADR-0004](../spec/adrs/ADR-0004-cli-exit-code-categories.md).

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

Per [ADR-0003](../spec/adrs/ADR-0003-unified-socket-resolution.md), every
subcommand resolves its target socket the same way:

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

See [ADR-0004](../spec/adrs/ADR-0004-cli-exit-code-categories.md) and
ZAZ-005 for the rationale.

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
| `-c`, `--config` `<CONFIG>` | — | Configuration file path |
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
(Ctrl+C) or the daemon is asked to stop over its socket. Per ZAZ-002, this
mode never daemonizes; use `zaz start` to launch a background daemon.

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
group when no name is given. Per ZAZ-003, the daemon executes the same
startup hook chain it ran at boot, so `restart` is the supported way to
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
running daemons stay up if their definitions did not change. Per ZAZ-003,
reload does not re-run startup tasks — use `zaz restart` for that.

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
