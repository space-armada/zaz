# HTTP service with a readiness gate

An API and the frontend that talks to it. `depends_on` alone waits for the
API process to spawn, which happens well before it binds a port. The `seed`
task in the `web` group would then fire its request into a connection
refused. `ready_check` makes the wait mean what the dependency was always
trying to say.

## Features used

- `ready_check = "curl -sf http://localhost:${api_port}/healthz"` asks the
  API itself. `-sf` is what makes it a check: `-f` turns a 4xx or 5xx into a
  nonzero exit, and `-s` keeps a progress meter out of the probe's output.
- `ready_timeout = "20s"` narrows the 30-second default. This binary starts
  in under a second, so twenty is already generous, and a shorter window
  reports a wedged server sooner.
- No `ready_poll_interval`. A curl against localhost is cheap enough to run
  at the 100ms default, and the default is what gets the service into
  `Running` fastest.
- `depends_on = ["api"]` on the `web` group is the payoff. The group waits
  for `api` to reach `Ready`, and `api` cannot reach `Ready` until the check
  passes.
- `${api_port}` comes from `[variables]`, so the port the server binds and
  the port the check probes cannot drift apart.

## Choosing a check

The check decides what `Ready` means for everything downstream, so a check
that passes before the service can serve is worse than no check at all. It
moves the race rather than removing it.

Probe the thing dependents actually use. A `/healthz` route the server
registers last is a real answer. These are not:

```sh
nc -z localhost 8080            # the port is bound before any route exists
pgrep -f bin/api                # the process is up; that was never the question
sleep 3 && true                 # a guess dressed as a check
```

A check that talks to a dependency of its own, such as a route that queries
the database, reports readiness for that too. Whether you want it to is a
judgement call about what the dependents need.

## Try it

```sh
zaz check                     # validate the config without running anything
zaz                           # default mode: TUI with the watcher attached
zaz status                    # api reads starting, then running
zaz restart api               # web waits again on the fresh check
```

The API's own log names the check once, then reports either `ready after
0.42s` or a give-up line carrying what the last probe printed. Individual
probes stay out of the log.

## See also

- [../docker-service-readiness/](../docker-service-readiness/README.md) —
  the same gate against a container's own health check.
- [../multi-group-dependencies/](../multi-group-dependencies/README.md) —
  cross-group `depends_on` without a readiness check.
- [../../configuration.md](../../configuration.md) — full project config
  reference, including what a readiness timeout does to dependents.
