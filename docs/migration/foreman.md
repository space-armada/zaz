# Workflow mapping: foreman, overmind, Procfile

zaz is not a Procfile runner. The overlap with foreman, overmind, and the
broader Procfile ecosystem is narrow: "run a named set of long-running
processes from one config." Everything else — formation control, `$PORT`
allocation, `.env` file loading, `Procfile.export` to systemd/upstart — is
out of scope. This page documents the part that does map.

If your motivation for using foreman or overmind was file-watching with a
restart, see [modd.md](modd.md) instead. zaz's daemons restart on file
changes per group; Procfile runners do not.

## Procfile to zaz

A Procfile is a list of `name: command` lines. Each line becomes a
`[[group.daemon]]` inside a single group whose `patterns = []` so it
runs on startup and stays up:

```text
# Procfile
web: bundle exec puma -p 3000
worker: bundle exec sidekiq
clock: bundle exec clockwork config/clock.rb
```

```toml
# zaz.toml
[[group]]
name = "procfile"
patterns = []

  [[group.daemon]]
  name = "web"
  command = "bundle exec puma -p 3000"

  [[group.daemon]]
  name = "worker"
  command = "bundle exec sidekiq"

  [[group.daemon]]
  name = "clock"
  command = "bundle exec clockwork config/clock.rb"
```

`patterns = []` is the standalone-daemon pattern: no file watching, the
daemons start with the rest of the daemon set and stay up until zaz
exits. The validator rejects empty groups, so the daemons are required.

## Command mapping

| foreman / overmind | zaz |
|--------------------|-----|
| `foreman start` | `zaz daemon` (foreground) or `zaz start` (background) |
| `foreman start NAME` | not directly supported; run `zaz` and start with one group via `depends_on` separation |
| `foreman run CMD` | `zaz task` if the command is one of the configured tasks; otherwise run it directly |
| `foreman check Procfile` | `zaz check` |
| `overmind start` | `zaz daemon` |
| `overmind restart NAME` | `zaz restart NAME` (operates on a group, not an individual daemon) |
| `overmind connect NAME` | not supported; zaz does not expose a per-daemon TTY connect surface |

`zaz restart` operates on a group, not an individual daemon. Split
processes that need independent restart cycles into separate groups
with no `depends_on` between them.

## Environment variables

foreman loads a `.env` file by default and supports `$PORT` allocation
across processes. zaz does neither. Use the explicit `[variables]` and
`env` tables described in [../configuration.md](../configuration.md):

```toml
[variables]
port = "3000"

[[group]]
name = "procfile"
patterns = []

  [group.env]
  RACK_ENV = "development"

  [[group.daemon]]
  name = "web"
  command = "bundle exec puma -p ${port}"
```

If you need values from a `.env` file, source them in your shell before
launching zaz, or template them into the `[variables]` table at config
generation time.

## What zaz does not provide

- **Process replica counts.** `foreman start -m web=2,worker=3` has no
  zaz equivalent. Each `[[group.daemon]]` is exactly one process.
- **`$PORT` macro.** zaz does not allocate ports across daemons.
- **`.env` file auto-loading.** Use `[variables]` and `env` explicitly.
- **`Procfile.export` to systemd / upstart.** zaz expects to be the
  long-running process; for service-manager integration, run
  `zaz start` from a systemd unit (the `pty-less-environment` example
  shows the relevant config flags).
- **Per-process attach / connect.** overmind's `overmind connect NAME`
  attaches to a tmux pane; zaz does not start a tmux session per
  daemon. Use the TUI ([../tui.md](../tui.md)) for live log viewing.

## See also

- [modd.md](modd.md) — full migration guide for users who actually want
  file-watching restarts.
- [../examples/pty-less-environment/](../examples/pty-less-environment/README.md)
  — the closest existing example to a service-manager-friendly daemon
  set.
- [../configuration.md](../configuration.md) — `[variables]`, `env`,
  `signal`, and `delay` reference.
