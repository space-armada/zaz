# Project configuration

Reference for the project-level zaz config file (`zaz.toml` or `zaz.json`).
This page documents every field, default, and validation rule. For per-user
preferences, see [user-configuration.md](user-configuration.md).

## Overview

A project config is committed alongside the project it controls. It declares
the watch groups, tasks, services, and global settings that zaz will operate
on. TOML and JSON forms describe the same schema; the only differences are
the table syntax and a small set of singular/plural aliases listed below.

Throughout this page, fields are introduced as TOML keys
(e.g. `[settings]`, `[[group]]`); the JSON spelling is noted alongside the
relevant alias.

## File discovery

zaz walks upward from the current working directory looking for `zaz.toml`
first, then `zaz.json`, and stops at the first directory holding either. The
walk gives up at the filesystem root. So a command issued from anywhere inside
a project resolves that project's config. Override discovery with the global
`--config` flag (see [cli.md](cli.md#global-flags)); when `--config` points at
a file, the format is detected from the extension (`.toml` or `.json`).

The daemon socket is derived from whichever config that walk found, so every
subcommand agrees on both the config and the daemon it addresses. See
[Socket and config resolution](cli.md#socket-and-config-resolution).

## Schema validation policy

Project config rejects unknown keys at every level, in TOML and JSON. Each
unknown key is a validation error with the code `unknown_key` and a
did-you-mean hint. TOML errors carry a line and column. Every unknown key in
the file is reported in one pass. There is no `version` field; schema
evolution is backwards-compatible by policy. See the schema-evolution
rationale in `docs/architecture.md`.

Older zaz versions dropped an unknown top-level or group-level TOML key
without a word. A config with a misspelled key such as `env_flie` loaded and
ignored it. That config now fails to load until the key is fixed. Run
`zaz check` after upgrading to find any such key.

A config that uses `env_file`, `env_command`, `env_watch`, `env_passthrough`,
or `env_command_timeout` needs a zaz release after v2026.7.1. The first such
release is the minimum version. An older zaz drops these keys from a TOML
config without an error and starts every process with no loaded environment.
Check the version before you share a config that uses them.

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
| `[[group.service]]` | `"services"` | Long-running services within a group. |

## `[settings]`

Global settings that apply to the whole config.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `shell` | string | unset | Shell used to run task/service commands. Falls back to `$SHELL` when unset. `env_command` falls back to `/bin/sh` instead. |
| `debounce` | duration | `100ms` | File-change batching window. Alias: `debounce_ms`. |
| `log_format` | enum | `pretty` | Log output format. See [`log_format`](#log_format) below. |
| `env_file` | string | unset | `KEY=VALUE` file loaded into every process. See [Environment loading](#environment-loading). |
| `env_command` | string | unset | Command whose JSON output is loaded into every process. |
| `env_watch` | array of string | empty | Files whose changes re-run `env_command`. Requires `env_command`. |
| `env_passthrough` | array of string | empty | Extra inherited variable names or `PREFIX_*` patterns that the base environment keeps. |
| `env_command_timeout` | duration | `120s` | Deadline for each `env_command`. Requires an `env_command` somewhere in the config. |

`debounce` and `env_command_timeout` accept the forms described in
[Duration parsing](#duration-parsing). The `env_*` fields have their own
section, [Environment loading](#environment-loading). `[settings]` has no
inline `env` table.

```toml
[settings]
shell = "bash"
debounce = "200ms"
log_format = "json"
```

## `[variables]`

User-defined string variables for `${var}` substitution. Substitution applies
to task and service `command` strings, to the lifecycle hooks and
`ready_check`, and to `env_command`. It does not apply to `env`, `working_dir`,
`env_file`, or `env_watch` values. Those are always literal.

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

`env_command` expands the same way as `command`, with two differences. The
file-context built-ins `${zaz:files}`, `${zaz:dirs}`, and `${zaz:prefix}` are
rejected, since the command runs at load and never has changed files. A name
that `[variables]` does not define is a validation error, so `zaz check` finds
it without running anything. An `env_file` is never expanded. A `${` inside
one is a parse error.

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

A group pairs a set of file patterns with the tasks and services that
should react when those files change.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `name` | string | required | Unique within the config; non-empty. |
| `patterns` | array of glob | empty | Globs that select files to watch. |
| `ignore` | array of glob | empty | Globs that exclude files from `patterns`. |
| `depends_on` | array of string | empty | Names of other groups that must finish before this one runs. |
| `working_dir` | string | unset | CWD for tasks/services; defaults to the config file's directory. A relative path resolves against the config directory. |
| `env` | table | empty | Environment variables merged into every task and service in the group. |
| `env_file` | string | unset | `KEY=VALUE` file loaded into every task and service in the group. |
| `env_command` | string | unset | Command whose JSON output is loaded into every task and service in the group. Runs in the group's working directory. |
| `env_watch` | array of string | empty | Files whose changes re-run `env_command`. Requires `env_command`. |
| `tasks` | array | empty | See [Tasks](#tasks-grouptask). TOML alias: `[[group.task]]`. |
| `services` | array | empty | See [Services](#services-groupservice). TOML alias: `[[group.service]]`. |

A group with no `patterns`, no `tasks`, and no `services` is rejected by
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
| `working_dir` | string | inherits | Overrides the group's `working_dir` for this task. A relative path resolves against the config directory. |
| `env` | table | empty | Per-task variables; merged on top of the group's `env`. |
| `env_file` | string | unset | `KEY=VALUE` file loaded into this task. |
| `env_command` | string | unset | Command whose JSON output is loaded into this task. Runs in the task's working directory. |
| `env_watch` | array of string | empty | Files whose changes re-run `env_command`. Requires `env_command`. |

When `name` is unset, zaz derives a display name from `command` by taking
words up to the first character in `-`, `$`, `|`, `>`, or `<`. So
`cargo build --release` derives to `cargo build`, while `cargo --version`
and `cargo -V` both derive to `cargo` and would collide. The
duplicate-task-name validator detects this and points at the
`name` field as a fix.

## Services (`[[group.service]]`)

Services are long-running processes that zaz keeps alive and restarts on
relevant file changes.

The TOML table `[[group.daemon]]` and the JSON `daemons` key are accepted as
deprecated aliases for backwards compatibility. New configs should use
`[[group.service]]`.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `name` | string | derived | Same derivation rule as task `name`. |
| `command` | string | required | Shell command to run; non-empty. |
| `cleanup_command` | string | unset | Runs before every spawn, including the first. Clears state a prior run left behind. |
| `ready_check` | string | unset | Reports whether the service has finished starting. Zero exit means ready. Unset means spawning counts as ready. |
| `stop_command` | string | unset | Stops the service instead of signalling its process group, on restart and on shutdown alike. Conflicts with `signal`. |
| `kill_command` | string | unset | Force kills the service instead of sending SIGKILL once `stop_timeout` elapses. |
| `signal` | enum | `SIGTERM` | Signal sent on restart. See [`Signal`](#signal). |
| `stop_timeout` | duration | `10s` | How long to wait for an exit after the stop before force killing. Also bounds `stop_command` itself. |
| `ready_poll_interval` | duration | `100ms` | How long to wait between `ready_check` runs. Requires `ready_check`; must be non-zero. |
| `ready_timeout` | duration | `30s` | How long `ready_check` may keep failing before the service is treated as failed to start. Requires `ready_check`. |
| `no_pty` | bool | `false` | Disable PTY allocation. PTY is on by default so tools like `tailwind --watch` work. |
| `silence` | enum | `none` | TUI suppression level. See [`Silence`](#silence). |
| `delay` | duration | unset | Wait this long after preceding tasks before starting. Alias: `delay_ms`. |
| `working_dir` | string | inherits | Overrides the group's `working_dir` for this service. A relative path resolves against the config directory. |
| `env` | table | empty | Per-service variables; merged on top of the group's `env`. |
| `env_file` | string | unset | `KEY=VALUE` file loaded into this service and its hooks. |
| `env_command` | string | unset | Command whose JSON output is loaded into this service and its hooks. Runs in the service's working directory. |
| `env_watch` | array of string | empty | Files whose changes re-run `env_command`. Requires `env_command`. |

```toml
[[group.service]]
name = "server"
command = "./bin/server"
signal = "SIGUSR2"
delay = "500ms"
no_pty = true
```

### Lifecycle hooks

zaz spawns every service in its own process group and stops it by
signalling that group. A service that leaves the group — one that calls
`setsid`, a wrapper script, a `docker run` whose container belongs to the
Docker daemon — never receives those signals. `cleanup_command`,
`stop_command`, and `kill_command` replace the parts of that lifecycle that
assume a well-behaved local child. A service that sets none of them behaves
exactly as it always has.

Hook commands expand exactly what `command` expands: the `[variables]`
table and the `${zaz:*}` built-ins. The file-context built-ins
(`${zaz:files}`, `${zaz:dirs}`, `${zaz:prefix}`) are rejected in a hook, the
same as in `command`.

`cleanup_command` runs immediately before every spawn, including the very
first start of a freshly started group. There is no "restart only" mode:
zaz keeps no memory across runs, so a crashed daemon's leftover state looks
exactly like a first start.

`stop_command` replaces the signal everywhere a service is stopped: on
restart, on daemon shutdown, and on config reload. That is why setting
`signal` beside it is rejected rather than silently ignored.

A stop then proceeds in a fixed order:

1. The stop runs: `stop_command` if set, otherwise the configured signal.
2. `stop_timeout` starts. Nothing further happens if the service exits
   inside it.
3. When the window closes, a `stop_command` process still running is killed
   first, so it cannot outlive the stop it was supposed to perform.
4. Then the force kill: `kill_command` if set, otherwise SIGKILL to the
   service's process group.

Every hook is best-effort and never blocks the lifecycle step it belongs
to. A nonzero exit is reported and the start or stop proceeds anyway.

A hook whose variables fail to expand is reported and does not run.
`cleanup_command` is skipped in that case and the service starts. A stop or
kill hook falls back to the signal instead, so a typo in a variable name
cannot leave a service with nothing asking it to go down.

Each hook run reports itself in the service's own log as a header naming the
expanded command, the hook's own output, and a footer with its duration and
exit code. `silence` suppresses the output but not the header and footer, so
a suppressed hook's failure stays visible.

zaz observes only the local process it spawned. It knows whether that
process exited within `stop_timeout`; it cannot know whether a container or
a remote job behind it is actually gone. A `stop_command` carrying its own
internal timeout keeps that timeout, independent of `stop_timeout`, and
escalation is decided purely on the local process's exit timing.

Worked configurations for both cases:
[docker-service](examples/docker-service/README.md) and
[local-service-cleanup](examples/local-service-cleanup/README.md).

### Readiness checks

A service reads `Running` the instant it spawns. Spawned is not the same as
listening on a port, or done replaying a write-ahead log, or past a JVM
warmup. `ready_check` closes that gap. zaz runs it after the spawn and
treats the service as ready only once it exits zero. A service that sets no
check counts as ready the moment it starts, exactly as before.

A check expands what `command` expands: the `[variables]` table and the
`${zaz:*}` built-ins. The file-context built-ins (`${zaz:files}`,
`${zaz:dirs}`, `${zaz:prefix}`) are rejected, the same as in `command` and
in the lifecycle hooks. Expansion happens per probe rather than once per
window.

The first probe runs immediately. A service that is already up should not
have to sit through an interval before it can say so. After that,
`ready_poll_interval` is measured from the start of each probe rather than
from its end, which makes it a floor on the gap between starts. Probes never
overlap. A check slower than its own interval simply runs back to back.

`ready_timeout` bounds the whole window. A window always runs its check at
least once, so a timeout too short to contain a poll tick, zero included,
means run the check and give up rather than give up without running it.

A service reads `Starting` while its window is open. A zero exit promotes it
to `Running`. A window that closes without one marks it `Failed` and leaves
the process alone. Stopping it would feed it straight back into the restart
path, which would spawn it, arm a fresh window, and reach the same timeout
again. A slow-booting service is also usually still worth inspecting.

A check whose variables do not expand, or whose shell will not spawn, fails
the service immediately rather than polling to the timeout. Neither gets
better inside the window.

A group holds at `Running` until every service in it that declares a
`ready_check` has answered. Only then does it become `Ready`. Groups listing
it in `depends_on` wait on that, which is the point of the feature: a
dependent starts once the thing it depends on can actually take work. A
check that runs out its window makes its group `Failed` and every group
waiting behind it `Skipped`.

Only a group's first transition to `Ready` is gated. A group that has
already reached it keeps it while a crashed service respawns and checks
again, the same way it stays `Ready` through a restart backoff.

The service's own log names the check once per window, then carries either
the pass or the give-up. The give-up line quotes the last failing probe's
exit code and output. Individual probes stream nothing. A thirty-second
window at the default interval runs three hundred of them, and streaming
each one would bury the service's own output. `silence` has nothing to
suppress here.

Worked configurations for both cases:
[http-service-readiness](examples/http-service-readiness/README.md) and
[docker-service-readiness](examples/docker-service-readiness/README.md).

## Environment loading

Three sources can add to a process's environment, beyond the inline `env` table. All three are optional, and each
exists at `[settings]`, group, task, and service level.

- `env_file` names a `KEY=VALUE` file that zaz parses itself.
- `env_command` names a shell command that prints a JSON object of variables. `direnv export json` is the typical
  one. It replaces wrapping every `command` in `direnv exec . <command>`.
- `env_watch` lists files whose changes re-run an `env_command`.

Leave all of them unset and a process gets exactly the environment it always did: the daemon's inherited
environment, plus the inline `env` tables. Two other changes apply even to a config that sets none of the new fields.
See [Relative `working_dir`](#relative-working_dir) and [Schema validation policy](#schema-validation-policy).

The hook commands run in their service's environment. Those are `ready_check`, `stop_command`, `kill_command`, and
`cleanup_command`. They need no field of their own.

`zaz env` and the `zaz_env` MCP tool show which variable names the running config loaded and from which source. They
never show values. See [cli.md](cli.md#zaz-env) and [mcp.md](mcp.md).

### When sources load

zaz reads every `env_file` and runs every `env_command` when it loads or reloads the config. The values become part
of the state that a reload compares, so an edit to a file plus `zaz reload` rebuilds the groups it changes.
Sources are not read again at each spawn.

A load that runs a slow `env_command` takes a while. The daemon reports `Starting` until the load finishes, and
`zaz start` waits for it. A failed first load ends differently by launcher.

- `zaz start` prints the coded error, shuts the daemon down, and exits non-zero. Fix the cause and run `zaz start`
  again.
- TUI autostart, MCP autostart, and a workspace supervisor's member attach leave the daemon up. `zaz status` and the
  TUI show the failure as a `load_error`. Fix the cause, then run `zaz reload`.

A failed reload leaves the running config in place.

`zaz check` never runs an `env_command`. It parses each `env_file` that exists, with the same parser and error codes
as load. It lists what it did not evaluate in the `--json` field `not_evaluated`. In text mode it prints one
`note: not evaluated, ...` line on stderr for each kind that has entries, up to three lines:

- `missing`: an `env_file` that does not exist.
- `not_read`: a file that is not a regular file, such as a named pipe. Check never opens one.
- `not_run`: an `env_command`.

None of these affects `valid` or the exit code.

A mistyped committed `env_file`, such as `config/dev.evn`, looks the same to check as a gitignored `.env` that a fresh
clone lacks. Both pass `zaz check` as missing. The mistyped one fails at `zaz start`. Read the `missing` list in CI
output when a path should exist.

### Precedence

A process's environment is built in order. Each source applies its sets and unsets in sequence, and a later source
wins.

1. The starting environment. This is the base environment when any level in the process's chain sets `env_command`.
   Otherwise it is the daemon's inherited environment.
2. The `[settings]` level: `env_file`, then `env_command`.
3. The group level: `env_file`, then `env_command`, then the inline `env` table.
4. The task or service level: `env_file`, then `env_command`, then the inline `env` table.

The more specific level wins, as inline `env` always did. Within one level the file goes before the command, so a
command's `null` can unset a variable that the file set. A `null` removes the variable at that point. A later source
may set it again.

Each `env_command` runs in exactly the environment its output is applied to: the starting environment plus every
source above it, including the `env_file` at its own level. This is what makes direnv's output exact, since direnv
prints a difference from the environment it ran in. A lower command sees what an upper level loaded.

An inline `env` above a level's `env_command` is input to that command. The command can override it, because direnv
reverts or reloads any variable it manages. Put a hand-written override at or below the command's level.

Hooks and the process itself get the complete result. A variable that the daemon inherited and the base environment
lacks reaches neither.

### Base environment

A process whose chain sets `env_command` starts from a fixed base instead of the daemon's inherited environment. The
base gives the same result whichever shell started the daemon. Without it, `direnv export json` run from a daemon
launched in a direnv shell prints nothing, because that shell already had the variables.

The base keeps these variables from the daemon's environment:

- `HOME`, `PATH`, `USER`, `LOGNAME`, `SHELL`, `TERM`, `LANG`, `TMPDIR`, and `TZ`.
- `LC_*` and `XDG_*`. direnv finds its allow list under `XDG_DATA_HOME`.
- `SSH_AUTH_SOCK`, `DISPLAY`, `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, and `CI`.
- `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, and `ALL_PROXY`, and their lowercase forms.
- `SSL_CERT_FILE` and `SSL_CERT_DIR`.

Every other inherited variable is dropped at a level that uses `env_command`. That includes `KUBECONFIG` and
`AWS_PROFILE` from the shell that launched the daemon. zaz logs the dropped names at info level, once per load and
per level. It never logs values. `zaz env` lists them too.

`env_passthrough` in `[settings]` adds names to the base. An entry is a variable name or a `PREFIX_*` pattern.

```toml
[settings]
env_passthrough = ["KUBECONFIG", "OP_SESSION_*", "DIRENV_CONFIG"]
```

An entry only adds. It never removes a fixed name, so `env_passthrough = ["OP_*"]` cannot drop `PATH`. A bare `*` is
rejected with `invalid_env_passthrough`, since a pattern needs a prefix. Use it for agent or secret-manager state that
an `env_command` needs, such as `OP_SESSION_*`, or tool settings such as `MISE_DATA_DIR`.

The `DIRENV_*` bookkeeping variables direnv exports reach processes unchanged.

A daemon that an MCP client launched may not have `direnv` on its `PATH`. The base takes `PATH` from the daemon. The
command then fails with exit 127 and says so in its error. Start the daemon from a shell that has the tool, with
`zaz start`.

A daemon launched from a direnv shell carries that shell's `PATH` additions, and `.envrc` prepends them again.
The duplicate entries are harmless. Start the daemon from a plain shell when a stale entry matters.

### Working directories

Each path has one rule, and the rules differ on purpose.

| Source | Resolves against |
|--------|------------------|
| `env_file` at any level | The config directory. |
| `env_command` | The level's effective working directory. |
| `env_watch` | The `env_command`'s working directory. |

The effective working directory of `env_command` is the config directory for `[settings]`. For a group it is the
group's `working_dir`, or the config directory when that is unset. For a task or service it is its own `working_dir`,
or its group's.

`env_file` and `env_command` at one level can therefore read different directories. A group with
`working_dir = "services/api"` and `env_file = ".env"` reads `.env` in the config directory. Its `env_command` runs in
`services/api`. Write `env_file = "services/api/.env"` to read the service's own file.

direnv, mise, and Nix find their configuration from the directory they run in. Per-directory `.envrc` files in a
monorepo therefore work with no extra config. A command that needs another directory can say so itself, as in
`cd services/api && direnv export json` or `mise env --json -C services/api`.

`env_watch = [".envrc"]` in a group with `working_dir = "services/api"` watches `services/api/.envrc`. A path may leave
the config directory, as in `"../flake.nix"`.

zaz logs each resolved `env_file` and `env_watch` path at info level, and warns about a listed path that does not
exist, so a typo shows up.

When several levels use the same command text in the same directory, they share one evaluation. It runs once per load.
The commands run one at a time, ordered by level and then by config order. Worst-case load time is the sum of every
distinct command, one timeout each.

#### Relative `working_dir`

A relative `working_dir` on a group, task, or service resolves against the config directory. It used to resolve against
the directory the daemon started in. A config that relied on that now points somewhere else. Use `zaz check`, which
prints the absolute config path, to confirm which directory applies.

### `env_file` format

An `env_file` uses a portable subset of the dotenv format. dotenv has no specification, and its implementations
disagree on quoting, escapes, and expansion. zaz accepts only lines that mean the same thing in Docker Compose,
dotenvy, and python-dotenv. Every point where they disagree is a parse error that names the line and says how to fix
it. An error names the line and never quotes it.

- A line is blank, a full-line `#` comment, or `KEY=VALUE` with an optional `export` prefix and a space after it.
- Leading whitespace is allowed. Trailing whitespace is stripped. A trailing `\r` is stripped, so CRLF files load.
- A key matches `[A-Za-z_][A-Za-z0-9_]*`. Whitespace around `=`, a bare `KEY`, and a duplicate key are errors.
  `KEY=` sets an empty string.
- An unquoted value may not contain whitespace, `#`, quotes, `$`, or `\`. A comment after a value is an error.
  Move it to its own line.
- A single-quoted value is literal and stays on one line. It is how to write a `$`, as in `HASH='$2b$12$abc'`. A `\`
  inside single quotes is an error.
- A double-quoted value may span lines, which accepts PEM keys. Its only escapes are `\n`, `\"`, and `\\`. Any other
  escape is an error. A `$` inside double quotes is an error. Single-quote the value instead.
- The file must be UTF-8 with no NUL bytes. It may not exceed 1 MiB.
- A UTF-8 byte order mark is an error. zaz does not strip it, because the dialects disagree about it.

There is no expansion. A `.env` written for Compose with `DATABASE_URL=postgres://${DB_USER}@localhost` fails to
parse. It does not load the literal text. `${...}` inside an `env_file` is not a zaz variable.

A missing `env_file` fails load. A fresh clone without a gitignored `.env` fails until the file exists. For an optional
file, use an `env_command` such as `direnv export json` with `dotenv_if_exists .env.local` in the `.envrc`.

The daemon reads a named pipe, so a secret manager's mounted env file works. Each read has a fixed 120 second deadline.
`zaz check` does not open a pipe.

### `env_command` output

The command runs under `settings.shell` when set, else `/bin/sh`. It never reads the daemon's `$SHELL`, so the result
does not depend on which program launched the daemon. A `.zshenv` that prints to stdout would break a zsh shell
setting. The `command` and hook fields keep their own shell rule.

The command's stdout must be empty, whitespace only, or one JSON object whose values are strings or `null`.

- Empty output means no changes. direnv prints it when nothing differs.
- A string sets the variable.
- `null` unsets it.
- Anything else fails load: a non-object, a number or nested value, text that is not JSON, a duplicate key, an empty
  key, a key containing `=` or NUL, a value containing NUL, and invalid UTF-8.

zaz parses the output only after the command exits 0 and both pipes reach end of file. A timed-out or killed run is
never parsed. The error names the kind of failure and the key, and gives a position. It never echoes stdout, since
a command such as `sops -d .env` prints secrets.

`direnv export json`, `mise env --json`, and `dotenvx get --format json` all fit. `nix print-dev-env --json` does not,
because it nests each variable under a type. Reach a Nix dev shell through direnv's `use flake` instead.

The command runs in its own process group with stdin closed. A command that prompts, such as `op signin`, fails at
once. On timeout, on `Shutdown`, and when a background helper holds the output pipe past the deadline, zaz sends
SIGTERM to the group, waits two seconds, then sends SIGKILL. The default deadline is 120 seconds, sized for a cold
Nix evaluation. Set `env_command_timeout` in `[settings]` to change it. A cold evaluation that exceeds the default
needs a larger value. A command that leaves a background process holding stdout fails with `env_command_held_pipe`.
Redirect such helpers to `/dev/null`.

A non-zero exit fails load. The error carries the command's stderr. Stderr is shown verbatim, except that escape
sequences are stripped and the text is capped at 4096 bytes. A wrapper that echoes a token, or a shell with `set -x`,
would put it in the error. That error reaches CLI users and MCP clients. Keep secrets out of a command's stderr.
Stderr from a command that exits 0 is logged at a quiet level and is not an error.

For direnv, stderr is how a blocked `.envrc` is reported. The error carries direnv's own message, which names
`direnv allow`.

### Watching sources

zaz watches every `env_file` and every path in `env_watch`. A change re-evaluates the affected sources and everything
below them, then rebuilds only the groups whose environment changed. It does not re-read `zaz.toml`. Run `zaz reload`
for that. A group whose `patterns` also matched the same save runs once, after the new environment is applied.

```toml
[settings]
env_command = "direnv export json"
env_watch = [".envrc", "flake.nix", "flake.lock"]
```

With `env_watch` unset, a command re-runs only on `zaz reload`. `env_watch` takes literal files and no globs, so a
directory that the command writes, such as nix-direnv's `.direnv/`, is never watched by accident. Listing it at a level
with no `env_command` is `env_watch_without_command`.

Before each evaluation zaz hashes the content of the watched files. A save that changed nothing, a `git checkout`
that rewrote the same bytes, or a bare `touch` leaves the environment alone and runs no command. The comparison after
an evaluation still covers every variable.

When a watched refresh fails, the running environment stays in place. The failure shows as `env_refresh_error` in
`zaz status`, the TUI, and `zaz env`, until a refresh succeeds. A save of code files in the same batch still runs
against the running environment.

### Things to know

- **`direnv allow` is not watched.** direnv blocks an `.envrc` after every edit. The watched refresh then fails with
  direnv's "is blocked" message. `direnv allow` writes a file named by a hash, which no `env_watch` entry can list.
  Run `zaz reload` after `direnv allow`.
- **Use `zaz reload`, not `direnv reload`.** `direnv reload` with unchanged content does not refresh zaz. `zaz reload`
  does, and it also applies pending `zaz.toml` edits.
- **A no-op content edit still restarts.** A comment added to `flake.nix` changes the file, so the command re-runs.
  direnv's `DIRENV_DIFF` and `DIRENV_WATCHES` can change with it, which restarts the groups below that level. An
  `.envrc` comment edit restarts them only after `direnv allow` and a reload.
- **Do not list a file the command rewrites.** A file that the command writes on every run is only safe when it
  writes identical bytes. A one-time write, such as nix updating `flake.lock`, costs one extra run and possibly one
  extra restart.
- **A shared `.direnv/` can be raced.** Each workspace member runs its own daemon, and each daemon evaluates on its
  own. Two members that share an `.envrc` can write the same `.direnv/` cache at once. So can your shell and zaz.
  zaz serializes only the evaluations inside one daemon.
- **A missing `.envrc` reads as an empty environment.** `direnv export json` in a directory with no `.envrc` exits 0
  with no output. A typo in `working_dir` therefore loads nothing. `zaz env` lists the source with `produced no
  keys`. That is the signal to look for.
- **An MCP reload can outlast the host's tool timeout.** `zaz_reload_config` waits for the evaluation. A cold Nix run
  can exceed a host's limit, so the agent sees a failed call for a reload that went through. `zaz_status` and `zaz_env`
  show the outcome.
- **An older zaz silently drops the fields.** See [Schema validation policy](#schema-validation-policy) for the
  minimum version.

### Error codes

Codes appear in `zaz check --json`, in a `load_error`, and in reload results.

| Code | Meaning |
|------|---------|
| `unknown_key` | A key that no config table defines. |
| `env_command_timeout_without_command` | `env_command_timeout` is set and no `env_command` exists. |
| `env_watch_without_command` | `env_watch` at a level with no `env_command`. |
| `env_command_file_builtin` | `${zaz:files}`, `${zaz:dirs}`, or `${zaz:prefix}` in an `env_command`. |
| `env_command_undefined_variable` | An `env_command` uses a name that `[variables]` lacks. |
| `env_command_malformed_variable` | An `env_command` has a `${` with no closing brace. |
| `invalid_env_passthrough` | An `env_passthrough` entry is not a name or a `PREFIX_*` pattern. |
| `env_file_missing` | The file does not exist. |
| `env_file_permission_denied` | The file exists and the daemon cannot read it. |
| `env_file_is_directory` | The path names a directory. |
| `env_file_io` | Any other read failure. |
| `env_file_read_timeout` | A read outlasted the 120 second deadline, as with a named pipe that has no writer. |
| `env_file_too_large` | The file exceeds 1 MiB. |
| `env_file_byte_order_mark` | The file starts with a UTF-8 byte order mark. |
| `env_file_invalid_utf8` | The file is not UTF-8. |
| `env_file_nul_byte` | The file holds a NUL byte. |
| `env_file_missing_equals` | An entry has no `=`. |
| `env_file_invalid_key` | A key is not `[A-Za-z_][A-Za-z0-9_]*`. |
| `env_file_space_around_equals` | Whitespace around `=`. |
| `env_file_duplicate_key` | A key appears twice. The error names both lines. |
| `env_file_inline_comment` | A comment follows a value. |
| `env_file_unquoted_whitespace` | Whitespace in an unquoted value. |
| `env_file_unquoted_quote` | A quote character in an unquoted value. |
| `env_file_dollar_sign` | A `$` outside single quotes. |
| `env_file_backslash` | A backslash in an unquoted value. |
| `env_file_backslash_in_single_quotes` | A backslash in a single-quoted value. |
| `env_file_bad_escape` | An escape other than `\n`, `\"`, `\\` in a double-quoted value. |
| `env_file_trailing_content` | Content after a closing quote. |
| `env_file_unterminated_quote` | A quote that never closes. The error names the line where it opened. |
| `env_command_failed` | The command exited non-zero. The error carries its stderr. |
| `env_command_timeout` | The command outlasted `env_command_timeout`. |
| `env_command_held_pipe` | The command exited and a background process still held its output. |
| `env_command_stdout_too_large` | Stdout exceeded 1 MiB. |
| `env_command_stderr_too_large` | Stderr exceeded 1 MiB. |
| `env_command_malformed_output` | Stdout is not one flat JSON object. |
| `env_command_spawn_failed` | The shell could not start, such as a missing working directory. |
| `evaluation_cancelled` | `Shutdown` or Ctrl+C ended the evaluation. |
| `evaluation_panicked` | The evaluation thread panicked. |
| `evaluation_unavailable` | The evaluation thread could not start. |
| `parse_error` | `zaz.toml` could not be read or parsed. |
| `config_not_found` | The config file does not exist. |

While the daemon is `Starting`, a restart request answers `daemon_starting`. After a failed first load it answers
`first_load_failed`. While a start or reload is applying, it answers `daemon_busy`.

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

Signal names are serialized in uppercase. The default for service restart
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

Duration-typed fields (`debounce`, `delay`, `stop_timeout`,
`ready_poll_interval`, `ready_timeout`, `env_command_timeout`) accept three input forms:

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
| Empty service command field | `group '{g}': service '{n}' has empty {field}` |
| Duplicate service name | `group '{g}': duplicate service name '{n}'` |
| File-context built-in in a service command field | `group '{g}': service '{n}' {field} references ${b}, ...` |
| Conflicting stop mechanism | `group '{g}': service '{n}' sets both signal and stop_command` |
| Readiness tuning without a check | `group '{g}': service '{n}' sets {field} without a ready_check` |
| Zero readiness poll interval | `group '{g}': service '{n}' has a zero ready_poll_interval` |
| `env_command_timeout` with no `env_command` | `settings: env_command_timeout is set but no env_command exists to time` |
| `env_watch` with no `env_command` at the same level | `{level}: env_watch is set but the same level has no env_command to re-run` |
| File-context built-in in an `env_command` | `{field} references ${b}, which is only populated for file-change triggers ...` |
| Undefined variable in an `env_command` | `{field} references ${name}, which [variables] does not define` |
| Unclosed `${` in an `env_command` | `{field} has a '${' with no closing '}'` |
| Bad `env_passthrough` entry | `settings: env_passthrough entry '{entry}': {problem}` |
| Unknown key | `unknown key '{key}' in {table}` |

Service errors report `{field}` as the name of the offending field, so a
service carrying more than one command string points at the right one.

`stop_command` replaces the restart signal outright, so a service setting both
it and `signal` is rejected rather than having its signal silently ignored.

`ready_poll_interval` and `ready_timeout` only ever apply to a `ready_check`,
so either one set alone is rejected rather than accepted and ignored. A zero
`ready_poll_interval` is rejected separately, because it would run the check
back to back with no pause. A zero `ready_timeout` is allowed: it means one
check, then give up.

Unknown-dependency errors include a "did you mean '{x}'?" hint when a
group name within Levenshtein distance 2 exists, otherwise an
`available groups are: ...` hint listing up to four candidates.
Duplicate-task and duplicate-service errors append
`(use explicit 'name' field to disambiguate)` when the duplicate came
from name derivation rather than an explicit `name = "..."`.

## Log storage

Log storage is configured per operator, not per project. The daemon's
API-visible log stream — what `zaz_logs`, the TUI, and `zaz status` read
back — is bounded by an in-memory hot buffer in every mode and can
optionally be persisted to a SQLite database under
`$XDG_STATE_HOME/zaz/logs/`. Backend selection and retention limits live
in the user config: see
[user-configuration.md#log_storage](user-configuration.md#log_storage)
for the backend selector, hot-buffer bounds, the SQLite retention
fields, and the persistence contract (database location, group rename
semantics, degraded-mode behavior, and the distinction from the
daemon's debug log files).
