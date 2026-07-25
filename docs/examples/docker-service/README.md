# Docker-driven service

Rebuild an image on change and run it as a named container. `docker run` is
a client: the container it starts is owned by the Docker daemon and sits
outside the process group zaz tracks. A signal to that group stops the
client and leaves the container running. The lifecycle hooks give zaz
commands that address the container by name instead.

## Features used

- `stop_command = "docker stop ${container}"` replaces the restart signal.
  It applies on every restart, on daemon shutdown, and on config reload.
  Setting `signal` beside it is a validation error.
- `kill_command = "docker kill ${container}"` replaces the SIGKILL zaz
  would otherwise send once `stop_timeout` runs out.
- `cleanup_command` removes a container a previous crash left behind, which
  is what makes the fixed `--name` safe to reuse. It runs before every
  spawn, including the first, because a crashed daemon looks exactly like a
  first start. The trailing `|| true` keeps the log quiet on the common
  case where there is nothing to remove.
- `stop_timeout = "45s"` widens the 10-second default, since a container
  draining in-flight connections legitimately needs longer. It bounds
  `docker stop` itself too: a stop hook still running when the window
  closes is killed before `kill_command` runs.
- The `image` task restarting the `api` service is the ordinary
  task-then-service ordering inside a group, not anything Docker-specific.
  Rebuilding the image reruns the service against it.
- `${image}` and `${container}` come from `[variables]`, so the name used
  to run, stop, kill, and clean up cannot drift apart.

## What zaz can and cannot see

zaz waits on the local `docker run` client, not on the container. It knows
that client exited within `stop_timeout`; it does not know whether the
container is actually gone. `docker stop` carries its own internal timeout,
independent of this one. Escalation to `kill_command` is decided purely on
the client's own exit timing.

Each hook run reports itself in the service's own log: a line naming the
expanded command, the hook's own output, then a line with its duration and
exit code. Watch them in the TUI alongside the service they belong to.

## Try it

```sh
zaz check                     # validate the config without running anything
zaz                           # default mode: TUI with the watcher attached
zaz daemon                    # foreground daemon, no TUI
zaz restart api               # exercises stop_command then cleanup_command
```

## See also

- [../local-service-cleanup/](../local-service-cleanup/README.md) — the
  same hooks against a plain local service.
- [../docker-service-readiness/](../docker-service-readiness/README.md) —
  these hooks plus a check that waits for the container to report healthy.
- [../../configuration.md](../../configuration.md) — full project config
  reference, including the lifecycle hook ordering rules.
- [../../cli.md](../../cli.md) — every subcommand and flag.
