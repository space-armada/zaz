# Per-directory `.envrc` files in a monorepo

Two services, each with its own `.envrc`, and a shared one at the repository
root. Each group loads the environment of its own directory. No command names
a path.

```text
.
├── .envrc
├── zaz.toml
└── services/
    ├── api/
    │   └── .envrc
    └── web/
        └── .envrc
```

## The `.envrc` files

```sh
# .envrc at the repository root
export LOG_LEVEL="debug"
export API_URL="http://localhost:8080"
```

```sh
# services/api/.envrc
source_up
export DATABASE_URL="postgres://localhost/api_dev"
export PORT="8080"
```

```sh
# services/web/.envrc
source_up
export VITE_API_URL="$API_URL"
export PORT="5173"
```

`source_up` loads the parent `.envrc` first. Without it, direnv treats a child
`.envrc` as the whole environment and unloads what the root exported.

## Features used

- `working_dir = "services/api"` points a group at its service directory. A
  relative path resolves against the directory that holds `zaz.toml`.
- `env_command = "direnv export json"` on a group runs in that group's
  working directory. direnv loads the nearest `.envrc` from there.
- `env_watch = [".envrc", "../../.envrc"]` resolves against the same
  directory. The second entry watches the root file from two levels down. A
  change to the root `.envrc` re-runs both groups' commands.
- Each group's command runs once per load. Groups that shared a command and a
  directory would share the one run.

## Allowing the files

Each `.envrc` needs its own `direnv allow`. Run it in the root and in each
service directory, then `zaz reload`.

```sh
direnv allow . services/api services/web
zaz reload
```

A watched edit to an `.envrc` makes direnv block it again. zaz keeps the
running environment and records the failure. `zaz status` and `zaz env` show
it. Allow the file, then run `zaz reload`.

## A shared cache

Each service directory keeps its own `.direnv/` cache. Two processes that
evaluate the same directory can race on it, such as your shell and zaz, or two
workspace members. zaz runs the commands inside one daemon one at a time.

Do not list a file that the command rewrites in `env_watch`. A rewrite on every
run can trigger another evaluation.

## Try it

```sh
zaz check
zaz start
zaz env api                   # the names the api group loaded, and from where
```

## See also

- [../direnv/](../direnv/README.md) — a single `.envrc` at the root.
- [../multi-group-dependencies/](../multi-group-dependencies/README.md) — the
  same layout without per-directory environments.
- [../../configuration.md](../../configuration.md#working-directories) — how
  each path resolves.
