# Project configuration

Reference for the project-level zaz config file (`zaz.toml` or `zaz.json`).
This page documents every field, default, and validation rule. For per-user
preferences, see [user-configuration.md](user-configuration.md).

## Overview

A project config is committed alongside the project it controls. It declares
the watch groups, tasks, daemons, and global settings that zaz will operate
on. TOML and JSON forms describe the same schema; the only differences are
the table syntax and a small set of singular/plural aliases listed below.

Throughout this page, fields are introduced as TOML keys
(e.g. `[settings]`, `[[group]]`); the JSON spelling is noted alongside the
relevant alias.

## File discovery

zaz looks for `zaz.toml` first, then `zaz.json`, in the current working
directory. Search is CWD-only — there is no walk up the directory tree.
The first existing file wins. Override discovery with the global
`--config` flag (see [cli.md](cli.md#global-flags)); when `--config`
points at a file, the format is detected from the extension (`.toml` or
`.json`).

## Schema validation policy

Project config uses `deny_unknown_fields` on every struct: unknown keys
are rejected with a clear error. There is no `version` field; schema
evolution is backwards-compatible by policy. See the schema-evolution
rationale in `spec/phases/index.md`.

This is a deliberate contrast with [user config](user-configuration.md),
which is permissive and falls back to defaults on parse failure. Project
config is shared and machine-checked; user config is local preference.

## TOML/JSON aliases

The TOML form uses singular keys for the array-of-tables blocks. The JSON
form uses plural keys for the corresponding arrays. Both forms map to the
same internal schema; either spelling is accepted in either format.

| TOML (singular) | JSON (plural) | What it holds |
|-----------------|---------------|---------------|
| `[[group]]` | `"groups"` | Watch groups. |
| `[[group.task]]` | `"tasks"` | Run-to-completion tasks within a group. |
| `[[group.daemon]]` | `"daemons"` | Long-running daemons within a group. |

## `[settings]`

Global settings that apply to the whole config.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `shell` | string | unset | Shell used to run task/daemon commands. Falls back to `$SHELL` when unset. |
| `debounce` | duration | `100ms` | File-change batching window. Alias: `debounce_ms`. |
| `log_format` | enum | `pretty` | Log output format. See [`log_format`](#log_format) below. |

`debounce` accepts the forms described in [Duration parsing](#duration-parsing).

```toml
[settings]
shell = "bash"
debounce = "200ms"
log_format = "json"
```

## `[variables]`

User-defined string variables for `${var}` substitution inside any task or
daemon `command` (and inside group-level `working_dir` / `env` values).

```toml
[variables]
build_dir = "./build"
test_flags = "-v --race"
```

Substitutions use `${name}` syntax. Use `\$` to write a literal `$` that
is not treated as the start of a substitution.

### Built-in variables

In addition to user-defined variables, zaz expands the following names
automatically when a watcher fires:

| Variable | Expansion |
|----------|-----------|
| `${zaz:files}` | Space-separated, shell-quoted list of changed file paths. |
| `${zaz:dirs}` | Sorted, deduplicated parent directories of the changed files. |
| `${zaz:root}` | Path of the directory containing the loaded config file. |
| `${zaz:prefix}` | Deepest directory that is an ancestor of every changed file. |

`${zaz:root}` errors at expansion time if the runtime context has not
recorded a config root; in normal CLI use this is always set.

### Shell-quoting rules

Substituted values are left bare when every character matches
`[A-Za-z0-9_./-]`. Otherwise the value is wrapped in single quotes, with
embedded `'` escaped as `'\''`. The same rule is applied to each path
inside `${zaz:files}` and `${zaz:dirs}`.

```text
foo            -> foo
src/main.rs    -> src/main.rs
file with sp   -> 'file with sp'
it's mine      -> 'it'\''s mine'
```

## Groups (`[[group]]`)

A group pairs a set of file patterns with the tasks and daemons that
should react when those files change.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `name` | string | required | Unique within the config; non-empty. |
| `patterns` | array of glob | empty | Globs that select files to watch. |
| `ignore` | array of glob | empty | Globs that exclude files from `patterns`. |
| `depends_on` | array of string | empty | Names of other groups that must finish before this one runs. |
| `working_dir` | string | unset | CWD for tasks/daemons; defaults to the config file's directory. |
| `env` | table | empty | Environment variables merged into every task and daemon in the group. |
| `tasks` | array | empty | See [Tasks](#tasks-grouptask). TOML alias: `[[group.task]]`. |
| `daemons` | array | empty | See [Daemons](#daemons-groupdaemon). TOML alias: `[[group.daemon]]`. |

A group with no `patterns`, no `tasks`, and no `daemons` is rejected by
validation as empty.

```toml
[[group]]
name = "backend"
patterns = ["**/*.go", "go.mod", "go.sum"]
ignore = ["**/*_test.go", "**/testdata/**"]
working_dir = "./services/api"
env = { GOFLAGS = "-mod=readonly" }
```

## Tasks (`[[group.task]]`)

Tasks run to completion. Each task lives inside a group.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `name` | string | derived | Display name. Derived from `command` when unset; see below. |
| `command` | string | required | Shell command to run; non-empty. |
| `on_change_only` | bool | `false` | When `true`, the task does not run on initial startup, only on file changes. |
| `silence` | enum | `none` | TUI suppression level. See [`Silence`](#silence). |
| `working_dir` | string | inherits | Overrides the group's `working_dir` for this task. |
| `env` | table | empty | Per-task variables; merged on top of the group's `env`. |

When `name` is unset, zaz derives a display name from `command` by taking
words up to the first character in `-`, `$`, `|`, `>`, or `<`. So
`cargo build --release` derives to `cargo build`, while `cargo --version`
and `cargo -V` both derive to `cargo` and would collide. The
duplicate-task-name validator detects this and points at the
`name` field as a fix.

## Daemons (`[[group.daemon]]`)

Daemons are long-running processes that zaz keeps alive and restarts on
relevant file changes.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `name` | string | derived | Same derivation rule as task `name`. |
| `command` | string | required | Shell command to run; non-empty. |
| `signal` | enum | `SIGTERM` | Signal sent on restart. See [`Signal`](#signal). |
| `no_pty` | bool | `false` | Disable PTY allocation. PTY is on by default so tools like `tailwind --watch` work. |
| `silence` | enum | `none` | TUI suppression level. See [`Silence`](#silence). |
| `delay` | duration | unset | Wait this long after preceding tasks before starting. Alias: `delay_ms`. |
| `working_dir` | string | inherits | Overrides the group's `working_dir` for this daemon. |
| `env` | table | empty | Per-daemon variables; merged on top of the group's `env`. |

```toml
[[group.daemon]]
name = "server"
command = "./bin/server"
signal = "SIGUSR2"
delay = "500ms"
no_pty = true
```

## Enums

### `Silence`

Controls which output streams the TUI suppresses. Suppressed output is
still captured for the API and for log files.

| Value | Effect |
|-------|--------|
| `none` | No suppression; show all output (default). |
| `stdout` | Suppress stdout in the TUI. |
| `stderr` | Suppress stderr in the TUI. |
| `all` | Suppress both streams in the TUI. |

### `Signal`

Signal names are serialized in uppercase. The default for daemon restart
is `SIGTERM`.

| Value | Notes |
|-------|-------|
| `SIGTERM` | Polite termination request (default). |
| `SIGINT` | Equivalent to a Ctrl-C interrupt. |
| `SIGHUP` | Common "reload your config" convention. |
| `SIGKILL` | Cannot be caught; use sparingly. |
| `SIGQUIT` | Termination with a core dump on most systems. |
| `SIGUSR1` | User-defined; meaning is up to the program. |
| `SIGUSR2` | User-defined; meaning is up to the program. |

### `log_format`

| Value | Effect |
|-------|--------|
| `pretty` | Human-readable structured logs (default). |
| `plain` | Plain-text log lines without structured fields. |
| `json` | Newline-delimited JSON; one event per line. |

## Duration parsing

Duration-typed fields (`debounce`, `delay`) accept three input forms:

- A human-readable string parsed by [`humantime`](https://docs.rs/humantime):
  `"500ms"`, `"2s"`, `"1m30s"`, `"1s 500ms"`.
- A non-negative integer, interpreted as milliseconds. Negative integers
  are rejected.
- The legacy `*_ms` alias key (`debounce_ms`, `delay_ms`), which only
  accepts integer milliseconds. The aliases exist for backwards
  compatibility; new configs should prefer the human-readable form.

```toml
[settings]
debounce = "200ms"   # preferred
# debounce_ms = 200  # equivalent legacy form
```

## Validation rules

All validation runs at load time and collects every error it finds before
returning. Each error carries a code (used by `zaz check --json`) and a
human message.

| Rule | Error format |
|------|--------------|
| Empty group name | `group[{i}]: name cannot be empty` |
| Duplicate group name | `group[{j}]: duplicate name '{name}' (first defined at group[{i}])` |
| Group has no patterns and no commands | `group '{name}': has no patterns and no commands` |
| Unknown dependency | `group '{g}': depends_on references unknown group '{d}'` |
| Self-dependency | `group '{name}': cannot depend on itself` |
| Dependency cycle | `dependency cycle detected: a -> b -> c -> a` |
| Invalid glob in `patterns` | `group '{name}': invalid pattern '{p}': {err}` |
| Invalid glob in `ignore` | `group '{name}': invalid ignore pattern '{p}': {err}` |
| Empty task command | `group '{g}': task '{n}' has empty command` |
| Duplicate task name | `group '{g}': duplicate task name '{n}'` |
| Empty daemon command | `group '{g}': daemon '{n}' has empty command` |
| Duplicate daemon name | `group '{g}': duplicate daemon name '{n}'` |

Unknown-dependency errors include a "did you mean '{x}'?" hint when a
group name within Levenshtein distance 2 exists, otherwise an
`available groups are: ...` hint listing up to four candidates.
Duplicate-task and duplicate-daemon errors append
`(use explicit 'name' field to disambiguate)` when the duplicate came
from name derivation rather than an explicit `name = "..."`.
