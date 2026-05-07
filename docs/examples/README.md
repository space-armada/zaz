# Examples

Worked configurations for common development workflows. Every `zaz.toml`
in this directory is checked against the live schema by an integration
test, so the examples cannot silently fall out of sync.

| Example | What it shows |
|---------|---------------|
| [go-dev-server/](go-dev-server/README.md) | Format, test, build, then run a Go binary as a daemon. |
| [node-dev-server/](node-dev-server/README.md) | Typecheck on save and keep `vite` running with a custom debounce. |
| [multi-group-dependencies/](multi-group-dependencies/README.md) | Backend + frontend monorepo with cross-group `depends_on`. |
| [task-only-workflow/](task-only-workflow/README.md) | Lint and test loop with no daemons, suitable for pre-commit and CI. |
| [pty-less-environment/](pty-less-environment/README.md) | Headless setup with `no_pty`, `delay`, and JSON logs. |

For the full schema, see [../configuration.md](../configuration.md). For
subcommand and flag reference, see [../cli.md](../cli.md). Migration
guides for users coming from `modd` and process-manager-adjacent tools
live under [../migration/](../migration/README.md).
