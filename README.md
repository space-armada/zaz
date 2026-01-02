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

## TUI Options

```bash
zaz --full          # Full style: split panes with group tree + logs
zaz --minimal       # Minimal style: one pane per task
zaz --no-autostart  # Don't auto-start daemon when TUI starts
```

Press `F1`/`F2` to switch between Full and Minimal styles at runtime.

## User Configuration

User preferences are stored separately from project configuration at
`~/.config/zaz/config.toml` (following XDG Base Directory specification):

```toml
# Don't auto-start daemon when running TUI
no_autostart = false

# Disable blinking/animation effects
disable_animations = false

# Default TUI style: "full" or "minimal"
tui_style = "full"
```

These settings are optional - zaz works fine without a user config file.
CLI flags take precedence over user config values.

## Keyboard Shortcuts

### Navigation

| Key | Action |
|-----|--------|
| `j`/`k`, `↓`/`↑` | Move down/up |
| `h`/`l`, `←`/`→` | Move left/right |
| `Tab` | Switch focus/pane |
| `g`/`G` | Go to top/bottom of logs |
| `PgUp`/`PgDn` | Scroll logs by page |

### Actions

| Key | Action |
|-----|--------|
| `r` | Restart selected group |
| `R` | Restart all groups |
| `c` | Clear logs |
| `F` | Toggle follow mode |

### Search & Filter

| Key | Action |
|-----|--------|
| `/` | Start search |
| `f` | Start filter |
| `n`/`N` | Next/previous match |
| `Esc` | Clear search/filter |

### Style

| Key | Action |
|-----|--------|
| `F1` | Switch to Full style |
| `F2` | Switch to Minimal style |
| `[`/`]` | Previous/next page (Minimal, >6 tasks) |

### General

| Key | Action |
|-----|--------|
| `q` | Quit |
| `?` | Toggle help overlay |

## License

MIT
