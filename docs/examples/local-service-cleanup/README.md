# Local service that leaves state behind

No containers here. A plain local service hits the same two problems a
Docker-wrapped one does. It takes a lockfile it only releases on a graceful
exit, and it forks a worker that calls `setsid` and so leaves the process
group zaz tracks. A service that is force killed never releases the
lockfile, and a signal to the tracked group never reaches the escaped
worker.

## Features used

- `cleanup_command` removes the stale lockfile and reaps any worker still
  running under the old name. It runs before every spawn, including the
  first, since a crashed daemon is indistinguishable from a fresh start.
  A cleanup that fails is logged and the service starts anyway.
- `stop_command = "pkill -TERM -f 'app.indexer'"` reaches the escaped
  worker by command-line pattern rather than by process group. This is the
  local counterpart of a `docker stop`, and the reason the hooks are not
  Docker-specific.
- `stop_timeout = "30s"` gives the indexer time to finish the batch it is
  on and checkpoint. When the window closes with no `kill_command` set, zaz
  falls back to SIGKILL on the tracked process group.
- No `signal` field. `stop_command` replaces the restart signal outright,
  so setting both is rejected at load time.

## Picking a pkill pattern

`pkill -f` matches against the full command line, so a pattern that is too
loose can match unrelated processes, including your editor or the shell
that ran `zaz`. Prefer a pattern unique to the service, and check it before
committing it:

```sh
pgrep -af 'app.indexer'       # list exactly what the pattern would signal
```

## Try it

```sh
zaz check                     # validate the config without running anything
zaz                           # default mode: TUI with the watcher attached
zaz restart indexer           # exercises stop_command then cleanup_command
```

Each hook run reports itself in the service's own log: a line naming the
expanded command, the hook's own output, then a line with its duration and
exit code. Watch them in the TUI alongside the service they belong to.

## See also

- [../docker-service/](../docker-service/README.md) — the same hooks
  against a container.
- [../../configuration.md](../../configuration.md) — full project config
  reference, including the lifecycle hook ordering rules.
- [../../cli.md](../../cli.md) — every subcommand and flag.
