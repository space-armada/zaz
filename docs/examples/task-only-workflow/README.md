# Task-only workflow

A lint-and-test loop with no daemons. Useful as a local pre-commit check
or as the body of a CI job invoked through `zaz check` and `zaz task`.

`silence = "stdout"` keeps the noisy passing output off the TUI; the full
output is still captured to disk and reachable via `zaz logs` and the
unix-socket API.

## Features used

- A group with `tasks` and no `daemons`.
- Per-task `silence` to control TUI noise without losing the underlying
  log capture.
- Tasks declared as TOML array-of-tables so the order matches the
  declared order.

## Try it

```sh
zaz check                     # validate config
zaz task checks               # run the full chain once
zaz                           # watch mode: re-run the chain on every change
```

## See also

- [../../configuration.md](../../configuration.md) — task fields and the
  `silence` enum.
- [../../cli.md](../../cli.md) — `zaz check`, `zaz task`, exit codes.
