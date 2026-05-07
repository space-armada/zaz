# Headless / PTY-less environment

For containers, systemd units, and CI runners that lack a TTY. The daemon
runs without PTY allocation, the migration task gets a 500ms grace period
before the worker connects, and logs come out as newline-delimited JSON
for log aggregators.

## Features used

- `no_pty = true` disables PTY allocation. Use this when the host has no
  controlling terminal, or when a process misbehaves under a PTY.
- `delay = "500ms"` waits half a second after the migration task completes
  before launching the worker daemon.
- Per-daemon `[group.daemon.env]` table for environment variables scoped
  to a single daemon.
- `[settings] log_format = "json"` so each log event is a single JSON
  object per line, friendly to log shippers.
- `signal = "SIGINT"` so the worker receives the same signal it would
  from a Ctrl-C in development.

## Try it

```sh
zaz check
zaz daemon                    # foreground daemon, no TUI
zaz start                     # background daemon, suitable for systemd
```

## See also

- [../../configuration.md](../../configuration.md) — `no_pty`, `delay`,
  and the `Signal` and `LogFormat` enums.
- [../../cli.md](../../cli.md) — `zaz daemon` vs. `zaz start`.
