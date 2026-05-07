# Go dev server

Watch a Go service, format and test on every change, build a binary, and run
it as a daemon. Tasks inside a group run sequentially before the daemon
starts; if any task fails the daemon is not (re)started.

## Features used

- Sequential tasks followed by a long-running daemon in the same group.
- Glob `ignore` for test files and the build output directory.
- `${zaz:dirs}` to scope `go test` to the directories that actually changed.
- `on_change_only = true` on `test` so it skips the initial startup pass.
- `signal = "SIGTERM"` for graceful daemon shutdown on restart. PTY is on
  by default, which keeps colored output and any TTY-only behavior intact.

## Try it

```sh
zaz check                     # validate the config without running anything
zaz                           # default mode: TUI with the watcher attached
zaz daemon                    # foreground daemon, no TUI
zaz task server               # run the task chain once and exit
```

## See also

- [../../configuration.md](../../configuration.md) — full project config
  reference.
- [../../cli.md](../../cli.md) — every subcommand and flag.
