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

Bounds on the in-memory log buffer used by the daemon and the TUI.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `memory_limit` | size string | `"100MB"` | Total memory across all process logs; oldest evicted when approached. |
| `max_lines_per_process` | integer | `100000` | Hard cap on retained lines per process. |

`memory_limit` accepts case-insensitive `B`, `KB`, `MB`, and `GB`
suffixes — all 1024-based — as well as fractional values like `"1.5MB"`.
A bare integer is interpreted as a byte count. Whitespace between the
number and the suffix is allowed (`"100 MB"`). Unparseable values fall
back to the 100 MB default.

```toml
[log_storage]
memory_limit = "200MB"
max_lines_per_process = 50000
```
