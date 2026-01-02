# zaz

zaz :: putting the zaz in pizzazz

A modern file-watching task runner and process manager for development
environments, heavily inspired by `modd`.

## Quick Start

Create a `zaz.toml` in your project root:

```toml
[[group]]
name = "backend"
patterns = ["**/*.go"]

[[group.task]]
name = "build"
command = "go build -o ./bin/server ./cmd/server"

[[group.daemon]]
name = "server"
command = "./bin/server"
```

Run zaz:

```bash
zaz
```

## Minimal Configuration

The simplest valid configuration requires at least one group with a name and
either patterns or commands:

```toml
[[group]]
name = "example"
patterns = ["**/*.txt"]
```

Or in JSON:

```json
{
  "groups": [
    {
      "name": "example",
      "patterns": ["**/*.txt"]
    }
  ]
}
```

## Configuration Reference

See [docs/configuration.md](docs/configuration.md) for the complete
configuration reference.

## Commands

```bash
zaz                 # Start TUI mode (default)
zaz task            # Run task commands once and exit
zaz daemon          # Start as background daemon
zaz status          # Show daemon status
zaz restart [group] # Restart a group (or all)
zaz stop            # Stop the daemon
zaz ignores         # Show default ignore patterns
```

## License

MIT
