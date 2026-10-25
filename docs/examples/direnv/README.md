# direnv without `direnv exec`

A Go API whose environment lives in a `.envrc`. Without `env_command`, every
command needs a `direnv exec .` prefix, and so does every hook. A daemon
started from an MCP client or a different shell would otherwise miss the
variables entirely.

```toml
# Before: the wrapper is repeated on every command.
[[group.service]]
name = "api"
command = "direnv exec . ./bin/api"
```

`env_command` loads the environment once, for every process:

```toml
[settings]
env_command = "direnv export json"
env_watch = [".envrc"]
```

## The `.envrc`

```sh
# .envrc
export DATABASE_URL="postgres://localhost/app_dev"
export LOG_LEVEL="debug"
export PORT="8080"
PATH_add bin
```

Add `use flake` or `use mise` here to bring in a Nix or mise toolchain. The
same `env_command` covers both.

## Features used

- `env_command = "direnv export json"` prints the environment as a flat JSON
  object. zaz applies it to every task and service, and to their hooks.
- `env_watch = [".envrc"]` re-runs the command when the file changes. zaz
  rebuilds only the groups whose environment changed.
- No `env_file`. direnv reads `.env` itself with `dotenv`, if you want one.

## Allowing the `.envrc`

direnv refuses to evaluate an `.envrc` that you have not allowed. The command
then exits non-zero and zaz fails the load with direnv's own message, which
says to run `direnv allow`.

```sh
direnv allow
zaz reload
```

Run `zaz reload` after `direnv allow`. The allow list is a hashed file that
`env_watch` cannot name, so zaz does not see the change by itself. Every edit
to the `.envrc` needs the same two steps, because direnv blocks the file again.

Use `zaz reload` rather than `direnv reload`. The first refreshes zaz.

## How it differs from your shell

zaz runs `direnv export json` from a fixed base environment, not from the
environment of the shell that started the daemon. That keeps the result the
same whichever launcher started it. The base keeps `PATH`, `HOME`, the `XDG_*`
variables, and a few more. Other inherited variables, such as `KUBECONFIG`, do
not reach your processes. Add them with `env_passthrough` in `[settings]`. See
[Base environment](../../configuration.md#base-environment).

If `direnv` is not on the daemon's `PATH`, which can happen when an MCP client
launches it, the load fails with exit 127.

## Try it

```sh
zaz check                     # validates the config and runs no command
zaz start                     # waits while direnv evaluates
zaz env                       # key names each source loaded, never values
```

`zaz env` marks a source that loaded no keys as `produced no keys`. That is
what a missing `.envrc` looks like, since direnv exits 0 and prints nothing
there.

## See also

- [../monorepo-envrc/](../monorepo-envrc/README.md) — one `.envrc` per service
  directory.
- [../../configuration.md](../../configuration.md#environment-loading) — the
  full reference.
