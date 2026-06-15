# User configuration

Reference for the per-user zaz config file. User config is local operator
preference; project config is shared and committed. For project config, see
[configuration.md](configuration.md).

## Overview

User config controls how the local zaz session presents and stores logs,
which TUI style opens by default, whether desktop notifications fire, and
whether the daemon is autostarted before the TUI. Every field is
optional; the file itself is optional. zaz reads it once at startup and
falls back to defaults whenever a value is missing.

The format is TOML.

## File discovery

User config resolves in this order:

1. `$XDG_CONFIG_HOME/zaz/config.toml` when `XDG_CONFIG_HOME` is set.
2. `$HOME/.config/zaz/config.toml` otherwise.
3. `./config.toml` as a last-resort fallback.

The first existing file wins. A missing file is silent — zaz uses
defaults without warning.

## Parsing semantics

Unlike project config, user config does not use `deny_unknown_fields`;
unknown keys are tolerated. Parse failures fall back to defaults with a
warning logged at startup. This is an intentional contrast with project
config, which fails closed on invalid input. The intent is that a stale
or malformed user config should never block zaz from starting on someone
else's machine — if the file does not parse, the operator gets a warning
and a working session, not a hard error.

## Top-level fields

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `no_autostart` | bool | `false` | When `true`, zaz does not autostart a daemon before opening the TUI. |
| `disable_animations` | bool | `false` | When `true`, blinking and animated effects in the TUI are skipped. |
| `tui_style` | enum | unset | Preferred TUI style; see below. |

`tui_style` accepts `"full"`, `"multi_pane"`, or the legacy alias
`"minimal"` (treated as `"multi_pane"`).

```toml
no_autostart = true
disable_animations = true
tui_style = "multi_pane"
```

## `[log_colors]`

Controls how log lines are colorized in the TUI.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `preserve_ansi` | bool | `true` | Pass through ANSI escape sequences from commands; disable to rely on `rules`. |
| `rules` | array of `{ pattern, color }` | four built-ins (below) | Pattern-to-color rules, applied in order. |
| `parse_json` | bool | `false` | Parse log lines as JSON when possible and pull out structured fields. |
| `json_level_field` | string | `"level"` | JSON field used to determine severity when `parse_json` is on. |
| `json_message_field` | string | `"msg"` | JSON field used as the rendered message when `parse_json` is on. |

Setting `rules` in user config **replaces** the built-in defaults. To
keep the defaults and add your own, copy the four entries below into your
config and append.

### Default rules

| Pattern | Color |
|---------|-------|
| `(?i)\berror\b` | `red` |
| `(?i)\bwarn(ing)?\b` | `yellow` |
| `(?i)\binfo\b` | `green` |
| `(?i)\bdebug\b` | `gray` |

Recognized color names: `red`, `green`, `yellow`, `blue`, `magenta`,
`cyan`, `white`, `gray`.

```toml
[log_colors]
preserve_ansi = true
parse_json = true
json_level_field = "severity"
json_message_field = "message"

[[log_colors.rules]]
pattern = "(?i)\\bfatal\\b"
color = "magenta"
```

## `[notifications]`

Desktop notification preferences. `enabled` is the master switch: when
`false`, nothing fires regardless of the other flags.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `enabled` | bool | `false` | Master switch for desktop notifications. |
| `on_failure` | bool | `true` | Notify when a task or daemon exits with a failure. |
| `on_success` | bool | `false` | Notify when a task completes successfully. |
| `on_group_complete` | bool | `true` | Notify when every group has reached a steady state. |

```toml
[notifications]
enabled = true
on_failure = true
on_success = false
on_group_complete = true
```

## `[log_storage]`

Controls how the daemon stores the API-visible log stream. Two backends
are available. `memory` keeps every retained line in a bounded in-process
buffer and loses history when the daemon exits. `sqlite` keeps the same
hot buffer for the live TUI and broadcast subscribers, but routes
historical pagination, search, and total counts through a persistent
SQLite database so `zaz_logs` and the daemon API return pre-restart
lines. The query shape (`name`, `offset`, `limit`, `search`) is identical
across both modes.

The hot buffer is always present. Under `sqlite`, the hot buffer covers
the recent tail served by `get(name, limit)` and the broadcast channel
that drives the TUI; SQLite serves everything else.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `backend` | enum | `"memory"` | Storage backend: `"memory"` or `"sqlite"`. The SQLite backend is opt-in. |
| `hot_memory_limit` | size string | `"100MB"` | Total in-memory budget across all process logs; oldest evicted when approached. Accepts the legacy alias `memory_limit`. |
| `hot_max_lines_per_process` | integer | `100000` | Hard cap on lines retained in the hot buffer per process. Accepts the legacy alias `max_lines_per_process`. |

Size strings accept case-insensitive `B`, `KB`, `MB`, and `GB`
suffixes — all 1024-based — as well as fractional values like `"1.5MB"`.
A bare integer is interpreted as a byte count. Whitespace between the
number and the suffix is allowed (`"100 MB"`). Unparseable values fall
back to the documented default.

The hot-buffer defaults do not shrink when SQLite is enabled; they bound
RAM in both modes. The SQLite limits bound disk independently.

```toml
[log_storage]
backend = "memory"
hot_memory_limit = "200MB"
hot_max_lines_per_process = 50000
```

### `[log_storage.sqlite]`

Persistent retention bounds for the SQLite backend. These fields are
honored when `backend = "sqlite"`; they are parsed but ignored under
`backend = "memory"`.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `max_size` | size string | `"512MB"` | Maximum on-disk database size; oldest rows pruned when approached. |
| `max_lines_per_process` | integer | `250000` | Hard cap on persisted lines per process. |

```toml
[log_storage]
backend = "sqlite"

[log_storage.sqlite]
max_size = "1GB"
max_lines_per_process = 500000
```

#### Database location

The database lands under
`$XDG_STATE_HOME/zaz/logs/<config-hash>.sqlite3`, falling back to
`~/.local/state/zaz/logs/<config-hash>.sqlite3` when `XDG_STATE_HOME`
is unset. The hash is keyed by the canonicalized project config path,
so unrelated projects never share log history. The directory tree is
created with `0o700` permissions on first use.

The database does not follow the socket's `.zaz/` directory fork even
when the socket is forced into a project tree; persistent log state
always lives under XDG state alongside the existing daemon debug logs.
This keeps SQLite WAL/SHM siblings out of project directories and out
of network-mounted source trees.

#### Persistent retention

The SQLite backend enforces two limits: the on-disk database size from
`max_size` and the lines-per-process cap from `max_lines_per_process`.
Both run twice: once immediately after every successful batch write so
fast writers settle near their budget, and again on a bounded periodic
cadence so processes that have stopped emitting still get trimmed.
Pruning issues plain `DELETE` statements; no `VACUUM` or
`wal_checkpoint` runs on every sweep, so freed pages are reused by
subsequent inserts and ingestion latency stays bounded. Age-based
retention is not implemented in this release.

`PRAGMA wal_checkpoint(TRUNCATE)` runs once on clean shutdown so the
`.sqlite3` file is self-contained for backup and reopen-startup cost
stays predictable.

#### Group rename semantics

`group_name` is a write-time snapshot: each row records the group the
process belonged to when the line was written. Renaming a group in
`zaz.toml` does not rewrite historical rows. A query filtered by the
new group name returns only rows written after the rename, and vice
versa. To inspect logs from before a rename, query by process name or
by the original group name.

#### Degraded mode

When SQLite fails after startup — a corrupt page, a held lock, a write
that hits a constraint — the failure surfaces as a
`LogStorageError` through the trait. Query errors reach the API as
`log query failed: …`; write errors are logged via `tracing::error!`
from the daemon's drain sites. The daemon does not exit. The hot
buffer and broadcast subscribers remain authoritative for the live
path, so the TUI and recent-tail `get(name, limit)` keep working
while persistence is degraded. Once the underlying issue clears, the
next batch write or query succeeds with no operator restart needed.

#### Distinction from debug log files

Persistent API logs are separate from the daemon's debug log files
(`daemon-output.log`, `daemon-debug.log`, `tui-debug.log`) described
in [cli.md](cli.md#log-files-and-rotation). The debug files capture
unstructured operator output and panics; the SQLite database holds
the structured, queryable per-process log stream that `zaz_logs`, the
TUI, and the daemon API consume.
