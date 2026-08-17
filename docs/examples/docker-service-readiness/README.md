# Container gated on its own health check

`docker run` returns once the container is started. Started says nothing
about the server inside it, and zaz cannot see inside the container to find
out. An image that declares a `HEALTHCHECK` has already answered the
question; this config reads Docker's own verdict rather than inventing a
second one.

The lifecycle hooks here are the same ones
[docker-service](../docker-service/README.md) explains, and for the same
reason: the container belongs to the Docker daemon, not to the process group
zaz tracks. A `docker run` service wants both sets of fields.

## Features used

- `ready_check` reads `{{.State.Health.Status}}`, which is `starting` until
  the image's own `HEALTHCHECK` passes and `healthy` afterwards. `grep -qx
  healthy` matches the whole line, so `unhealthy` does not read as a pass.
  An image with no `HEALTHCHECK` reports an empty status and never passes;
  probe the service directly with `docker exec` in that case.
- `ready_poll_interval = "1s"` slows the 100ms default. Each probe is a full
  `docker inspect` round trip to the Docker daemon, and ten of them a second
  buys nothing against a health check that reruns on its own interval
  anyway.
- `ready_timeout = "90s"` widens the 30-second default, because the window
  has to cover the entrypoint, the process boot, and the image's own
  `--start-period` before the first health check even runs.
- `cleanup_command`, `stop_command`, `kill_command`, and `stop_timeout`
  address the container by name instead of by signal. See
  [docker-service](../docker-service/README.md) for why each one is there.
- `${image}` and `${container}` come from `[variables]`, so the name used to
  run, inspect, stop, kill, and clean up cannot drift apart.

## Reading a window that closed

A container that never reports healthy leaves the service `Failed` with the
give-up line quoting the last probe. That probe exits nonzero with nothing
on stdout, since `grep -q` is quiet by design, so the line names an exit code
and no more. The verdict Docker recorded is one command away:

```sh
docker inspect -f '{{json .State.Health}}' myapp-api
```

That returns the last few health-check runs with their output and exit
codes, which is the detail the readiness log deliberately does not carry.

The container keeps running. zaz gives up on waiting, not on the process, so
there is something left to inspect.

## Try it

```sh
zaz check                     # validate the config without running anything
zaz                           # default mode: TUI with the watcher attached
zaz status                    # api reads starting until the container is healthy
zaz restart api               # stop, clean up, run, then wait on the check again
```

## See also

- [../http-service-readiness/](../http-service-readiness/README.md) — the
  same gate against a local HTTP server, with a dependent group behind it.
- [../docker-service/](../docker-service/README.md) — the lifecycle hooks on
  their own, without a readiness check.
- [../../configuration.md](../../configuration.md) — full project config
  reference, including the readiness polling and timeout rules.
