# Migrating from modd

A guide for converting [modd](https://github.com/cortesi/modd) configs to
zaz. zaz cites modd as direct inspiration; phase 9 implemented the
parity features (`silence`, per-command `working_dir`, `delay`,
per-group/task/daemon `env`) that the trickier conversions rely on. The
guide below walks every modd directive that has a zaz equivalent and ends
with a [Behavioral differences](#behavioral-differences) section
covering the cases where translation changes runtime semantics.

The Complete example at the bottom converts to
[modd-example/zaz.toml](modd-example/zaz.toml), which is parsed by
`tests/example_configs.rs` against the live schema.

## Quick reference

| modd | zaz |
|------|-----|
| pattern block (`p1 p2 { … }`) | `[[group]]` with `name`, `patterns` |
| `prep:` | `[[group.task]]` |
| `prep +onchange:` | task with `on_change_only = true` |
| `prep +silent:` | task with `silence = "all"` |
| `daemon:` | `[[group.daemon]]` |
| `daemon +sigterm:` (or other `+sig*`) | daemon with `signal = "SIGTERM"` |
| `daemon -delay 500ms:` | daemon with `delay = "500ms"` |
| `indir:` | group or per-task/daemon `working_dir` |
| `!pattern` (negation) | `ignore = ["pattern"]` |
| `prep: \|` (multiline) | `command = """…"""` |
| `@var = …` (preamble) | `[variables]` table, `${var}` in commands |
| `@shell = bash` | `[settings] shell = "bash"` |
| `@mods` | `${zaz:files}` |
| `@dirmods` | `${zaz:dirs}` |
| `@confdir` | `${zaz:root}` |
| empty pattern (`{ daemon: … }`) | `[[group]]` with `patterns = []` |
| `modd -f path/to/conf` | `zaz --config path/to/zaz.toml …` |
| `modd -p` (run preps then exit) | `zaz task` |
| `modd` (default) | `zaz daemon` (foreground) or `zaz start` (background) |

## File discovery

modd reads `./modd.conf` by default and accepts `-f path` to point
elsewhere. zaz looks for `zaz.toml` first, then `zaz.json`, **only** in the
current working directory. There is no walk up the tree. For a
non-default path, pass the global `--config` flag described in
[../cli.md](../cli.md#global-flags); the format is detected from the
extension.

```sh
# modd
modd -f deploy/modd.conf

# zaz
zaz --config deploy/zaz.toml
```

## Patterns

modd patterns map directly to zaz's `patterns`. Both support `*`, `**`,
`?`, character classes, and brace expansion. Quoting rules differ at the
config layer, but the underlying glob syntax is the same.

```text
# modd
**/*.go {
  prep: go build
}
```

```toml
# zaz
[[group]]
name = "go-build"
patterns = ["**/*.go"]

  [[group.task]]
  command = "go build"
```

### Negation

modd uses the `!` prefix inline; zaz moves negations into a separate
`ignore` array:

```text
# modd
**/*.go !**/*_test.go {
  prep: go build
}
```

```toml
# zaz
[[group]]
name = "go-build"
patterns = ["**/*.go"]
ignore = ["**/*_test.go"]

  [[group.task]]
  command = "go build"
```

### Multiple blocks with the same patterns

modd allows several blocks to share patterns. zaz requires uniquely
named groups, so combine them or split them with `depends_on`:

```text
# modd
**/*.go {
  prep: go test ./...
}

**/*.go {
  prep: go build
  daemon: ./bin/server
}
```

```toml
# zaz, single group
[[group]]
name = "go"
patterns = ["**/*.go"]

  [[group.task]]
  command = "go test ./..."

  [[group.task]]
  command = "go build"

  [[group.daemon]]
  command = "./bin/server"
```

```toml
# zaz, separate groups with explicit ordering
[[group]]
name = "go-test"
patterns = ["**/*.go"]

  [[group.task]]
  command = "go test ./..."

[[group]]
name = "go-build"
patterns = ["**/*.go"]
depends_on = ["go-test"]

  [[group.task]]
  command = "go build"

  [[group.daemon]]
  command = "./bin/server"
```

## Tasks (`prep`)

`prep:` becomes `[[group.task]]`. Multiple `prep:` lines run sequentially
in modd; multiple `[[group.task]]` entries run sequentially in zaz, and
fail-fast in both. Two flags need translation:

| modd flag | zaz field |
|-----------|-----------|
| `+onchange` | `on_change_only = true` |
| `+silent` | `silence = "all"` (also `"stdout"`, `"stderr"`, `"none"`) |

```text
# modd
**/*.go {
  prep +onchange: go vet ./...
  prep +silent: go fmt @mods
}
```

```toml
# zaz
[[group]]
name = "go"
patterns = ["**/*.go"]

  [[group.task]]
  command = "go vet ./..."
  on_change_only = true

  [[group.task]]
  command = "go fmt ${zaz:files}"
  silence = "all"
```

`silence` accepts a finer split than modd's binary `+silent`: `"stdout"`
and `"stderr"` suppress one stream while letting the other through.
Suppressed output is still captured for the API and log file; the
filtering is at the TUI render layer.

## Daemons

`daemon:` becomes `[[group.daemon]]`. The signal flags map to the
`signal` field, which accepts the canonical signal name as a string.

```text
# modd
**/*.go {
  daemon +sigterm: ./bin/server
  daemon +sighup: ./bin/worker
}
```

```toml
# zaz
[[group]]
name = "go"
patterns = ["**/*.go"]

  [[group.daemon]]
  command = "./bin/server"
  signal = "SIGTERM"

  [[group.daemon]]
  command = "./bin/worker"
  signal = "SIGHUP"
```

Supported signal names: `SIGTERM` (zaz default), `SIGINT`, `SIGHUP`,
`SIGKILL`, `SIGQUIT`, `SIGUSR1`, `SIGUSR2`. See
[../configuration.md](../configuration.md) for the canonical list.

### Delay before daemon startup

modd's `-delay 500ms:` delays the daemon launch after preps complete.
zaz uses the `delay` field on the daemon, which takes the same
human-readable durations as `[settings] debounce`:

```text
# modd
**/*.go {
  daemon -delay 500ms: ./bin/server
}
```

```toml
# zaz
[[group]]
name = "go"
patterns = ["**/*.go"]

  [[group.daemon]]
  command = "./bin/server"
  delay = "500ms"
```

`delay` is per-daemon and gates the daemon's startup, not file-change
batching. The global `[settings] debounce` is the file-change batching
window.

## Variables

modd declares variables with `@name = value` at the top of the file and
expands them as `@name`. zaz uses a `[variables]` table and `${name}`
expansion. Both are global with no block scoping.

```text
# modd
@bin = ./node_modules/.bin

@bin/eslint src/**/*.js {
  prep: @bin/eslint @mods
}
```

```toml
# zaz
[variables]
bin = "./node_modules/.bin"

[[group]]
name = "eslint"
patterns = ["src/**/*.js"]

  [[group.task]]
  command = "${bin}/eslint ${zaz:files}"
```

Built-in variable mapping:

| modd | zaz |
|------|-----|
| `@mods` | `${zaz:files}` |
| `@dirmods` | `${zaz:dirs}` |
| `@confdir` | `${zaz:root}` |

`${zaz:files}` and `${zaz:dirs}` shell-quote their values, so paths with
spaces or shell metacharacters survive interpolation. modd's `@mods`
shell-quotes too; the behavior matches.

modd's `@shell = bash` preamble becomes `[settings] shell = "bash"`. See
[../configuration.md](../configuration.md#settings).

## Working directory

modd's `indir:` directive sets the working directory for every command
in the block. zaz supports `working_dir` at the group level (covers all
tasks and daemons) and at the per-task or per-daemon level (overrides
the group setting).

```text
# modd
ui/package.json {
  indir: ./ui
  prep: pnpm install
  daemon: pnpm run dev
}
```

```toml
# zaz, group-level
[[group]]
name = "ui"
patterns = ["ui/package.json"]
working_dir = "./ui"

  [[group.task]]
  command = "pnpm install"

  [[group.daemon]]
  command = "pnpm run dev"
```

modd allows multiple `indir:` directives within one block to target
different commands. In zaz, set `working_dir` on the individual task or
daemon:

```toml
[[group]]
name = "lint"
patterns = ["**/*.ts"]

  [[group.task]]
  command = "npm run lint"
  working_dir = "./frontend"

  [[group.task]]
  command = "npm run lint"
  working_dir = "./backend"
```

## Environment variables

modd inlines env vars into the command (`prep: ENV=test go test ./...`).
That continues to work in zaz commands, but zaz also has explicit `env`
tables on groups, tasks, and daemons. Per-task and per-daemon `env`
merges on top of group `env`.

```toml
[[group]]
name = "go-test"
patterns = ["**/*.go"]

  [group.env]
  GO_ENV = "test"

  [[group.task]]
  command = "go test ./..."

    [group.task.env]
    CGO_ENABLED = "0"
```

## Multiline commands

modd's `prep: |` shell block becomes a TOML multiline string:

```text
# modd
**/*.go {
  prep: |
    echo "Starting build..."
    go build -o bin/app
    echo "Done!"
}
```

```toml
# zaz
[[group]]
name = "go-build"
patterns = ["**/*.go"]

  [[group.task]]
  command = """
echo "Starting build..."
go build -o bin/app
echo "Done!"
"""
```

Both forms are passed to the shell as a single script.

## Standalone daemons

modd allows a block with no patterns and a single `daemon:` directive to
run a process for the lifetime of modd. zaz expresses the same idea with
`patterns = []`:

```text
# modd
{
  daemon: cloud-sql-proxy
}
```

```toml
# zaz
[[group]]
name = "cloud-sql-proxy"
patterns = []

  [[group.daemon]]
  command = "cloud-sql-proxy"
```

A group with empty `patterns` and no tasks is rejected as empty; once it
has at least one daemon or task it validates. The daemon starts with the
rest of the daemon set and stays up until zaz exits.

## Behavioral differences

Some directives have an obvious one-to-one mapping but the runtime
behavior changes after translation. These are the cases worth checking
during a migration.

### Default daemon signal

modd defaults to `SIGHUP` when no signal flag is given; zaz defaults to
`SIGTERM`. A modd `daemon: ./bin/server` translated as a zaz daemon with
no `signal` field will receive `SIGTERM` on restart instead of `SIGHUP`.
Set `signal = "SIGHUP"` explicitly to preserve the modd behavior.

### PTY allocation

zaz allocates a PTY for daemons by default; modd does not. A daemon
that misbehaves under a PTY (printing escape codes, refusing to start
without a TTY in the right state) needs `no_pty = true` to match modd's
behavior. The `pty-less-environment` example covers this case.

### Default ignore list

modd silently ignores VCS directories (`.git`, `.hg`, `.svn`),
`.DS_Store`, and editor swap files. zaz ignores nothing implicitly; a
glob like `**/*` will match `.git/HEAD` unless explicit `ignore`
patterns exclude it. Migrate the modd defaults you depend on into the
group's `ignore` array.

### One-shot vs always-on invocation

modd has two top-level modes: default (run preps, start daemons, watch),
and `-p` / `--prep` which runs preps and exits. The mapping:

| modd | zaz |
|------|-----|
| `modd` (default) | `zaz daemon` (foreground) or `zaz start` (background) |
| `modd -p` | `zaz task` |

`zaz daemon` runs in the foreground and never daemonizes; `zaz start`
launches the daemon as a detached background process suitable for
service managers. See [../cli.md](../cli.md) for the full subcommand
list.

### File discovery

Already covered above, but worth reiterating: zaz only searches the
current working directory for `zaz.toml` / `zaz.json`. modd's
`-c`/`--noconf` (don't watch the config file) has no zaz equivalent; zaz
does not auto-reload on config changes. Use `zaz reload` to apply config
edits to a running daemon, or restart it.

### Bell, notify, exec

Three modd CLI flags have no current zaz analogue:

- `-b` / `--bell` (terminal bell on prep failure)
- `-n` / `--notify` (system notifications on prep failure)
- `--exec` (run a single command in the built-in shell and exit)

The system-notification surface is on the user-config side; see
`[notifications]` in [../user-configuration.md](../user-configuration.md).

## Complete example

A Go monorepo with a database proxy, sqlc generator, and Vite-style UI.

### Original `modd.conf`

```text
{
  daemon: cloud-sql-proxy
}

**/*.go {
  prep: go test @dirmods
}

**/*.go !**/*_test.go {
  prep: make build
  daemon +sigterm: bin/ems server
  daemon +sigterm: bin/ems testfeeds
}

db/manual.sql {
  prep: make generate-sqlc
}

ui/package.json {
  indir: ./ui
  prep: pnpm install
}

ui/package.json {
  indir: ./ui
  daemon: pnpm run dev
}
```

### Converted `zaz.toml`

The two `**/*.go` blocks consolidate into named test and build groups
linked by `depends_on`, the two `ui/package.json` blocks fold into a
single `ui` group, and the standalone proxy uses empty patterns. The
final file lives at [modd-example/zaz.toml](modd-example/zaz.toml) and
is exercised by `tests/example_configs.rs`.

```toml
[[group]]
name = "cloud-sql-proxy"
patterns = []

  [[group.daemon]]
  name = "cloud-sql-proxy"
  command = "cloud-sql-proxy"

[[group]]
name = "go-test"
patterns = ["**/*.go"]

  [[group.task]]
  name = "test"
  command = "go test ${zaz:dirs}"

[[group]]
name = "go-build"
patterns = ["**/*.go"]
ignore = ["**/*_test.go"]
depends_on = ["go-test"]

  [[group.task]]
  name = "build"
  command = "make build"

  [[group.daemon]]
  name = "ems-server"
  command = "bin/ems server"
  signal = "SIGTERM"

  [[group.daemon]]
  name = "ems-testfeeds"
  command = "bin/ems testfeeds"
  signal = "SIGTERM"

[[group]]
name = "sqlc"
patterns = ["db/manual.sql"]

  [[group.task]]
  name = "generate"
  command = "make generate-sqlc"

[[group]]
name = "ui"
patterns = ["ui/package.json"]
working_dir = "./ui"

  [[group.task]]
  name = "install"
  command = "pnpm install"

  [[group.daemon]]
  name = "dev"
  command = "pnpm run dev"
```

## Tips

1. Name groups for what they do, not for the patterns that trigger
   them. modd encourages anonymous blocks; zaz requires explicit names
   and benefits from descriptive ones.
2. Convert one block at a time and run `zaz check` between conversions.
3. When two modd blocks share a pattern, prefer combining them into one
   zaz group. Reach for `depends_on` only when the ordering is between
   distinct concerns.
4. After conversion, re-read [Behavioral differences](#behavioral-differences)
   with the converted config in hand. The defaults that change silently
   (`signal`, PTY, ignore list) are easy to miss.

## See also

- [foreman.md](foreman.md) — Procfile / foreman / overmind workflow mapping.
- [../configuration.md](../configuration.md) — full schema reference.
- [../cli.md](../cli.md) — subcommands referenced above (`zaz check`,
  `zaz task`, `zaz daemon`, `zaz start`, `zaz reload`).
- [../examples/](../examples/README.md) — worked configs to crib from
  once the migration is done.
