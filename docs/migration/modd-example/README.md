# Worked example: modd → zaz

The end state of the "Complete example" walkthrough in
[../modd.md](../modd.md). A modd.conf with five blocks (a standalone
daemon, Go test/build/server pair, a sqlc generator, and a UI install/dev
pair) maps to five zaz groups in a single `zaz.toml`.

## Features used

- `patterns = []` standalone daemon for the always-on `cloud-sql-proxy`.
- `${zaz:dirs}` so `go test` runs against the directories that changed,
  matching modd's `@dirmods`.
- `ignore = ["**/*_test.go"]` to express modd's `!**/*_test.go` negation.
- `depends_on` to gate the build group behind the test group, since the
  two modd blocks shared `**/*.go`.
- `signal = "SIGTERM"` to match modd's `+sigterm` flag explicitly,
  rather than relying on either tool's default.
- Per-group `working_dir` for the `ui` group, replacing modd's `indir`.

## Try it

```sh
zaz check zaz.toml                   # validate this file alone
cargo test --test example_configs    # the migration test that backs this file
```

## See also

- [../modd.md](../modd.md) — the migration walkthrough this file ends.
- [../../configuration.md](../../configuration.md) — schema reference for
  every field used here.
