# Migration

Guides for moving an existing config to zaz. Each guide maps the source
tool's directives to zaz equivalents and calls out the behavioral
differences that survive the translation.

| Guide | Source tool | Coverage |
|-------|-------------|----------|
| [modd.md](modd.md) | [modd](https://github.com/cortesi/modd) | Required v1 coverage. zaz cites modd as inspiration; phase 9 implemented the parity features the guide leans on. |
| [foreman.md](foreman.md) | foreman / overmind / Procfile | Workflow mapping only. zaz is not a Procfile runner; the guide covers the overlap and is explicit about what does not translate. |

The worked example referenced from the modd guide lives at
[modd-example/zaz.toml](modd-example/zaz.toml) and is parsed by
`tests/example_configs.rs` against the live schema, so it cannot drift
from the implementation.

For schema and CLI details that the guides link out to, see
[../configuration.md](../configuration.md) and [../cli.md](../cli.md).
For configurations to crib from once the migration is done, see
[../examples/](../examples/README.md).
