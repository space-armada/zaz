# Multi-group with dependencies

A monorepo split into a Go `backend` and a TypeScript `frontend`. The
frontend group declares `depends_on = ["backend"]`, so its tasks and
daemon do not start until the backend group's tasks have completed and
its daemon is running.

## Features used

- Two groups, each with its own patterns, ignores, tasks, and daemon.
- Per-group `working_dir` so commands stay short and each group operates
  inside its own subtree.
- Cross-group ordering via `depends_on`.
- Independent file-watch scopes per group; backend changes do not retrigger
  frontend tasks unless the dependency chain requires it.

## Layout this example assumes

```text
.
├── backend/      # Go module rooted here
│   └── cmd/server
└── frontend/     # Node project rooted here
    └── src/
```

## Try it

```sh
zaz check
zaz                           # TUI shows both groups side by side
zaz status                    # snapshot of every group, task, and daemon
zaz reload                    # re-read this config without dropping daemons
```

## See also

- [../../configuration.md](../../configuration.md) — `depends_on`,
  `working_dir`, and validation rules for cycles and unknown dependencies.
- [../../cli.md](../../cli.md) — `zaz status`, `zaz restart`, `zaz reload`.
