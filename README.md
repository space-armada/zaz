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

Plain `zaz` opens the TUI. If a daemon is already running for the target
socket, the TUI reuses it; otherwise zaz auto-starts one unless you pass
`--no-autostart`.

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

## Commands

```bash
zaz                 # Open the TUI (reuses or auto-starts a daemon)
zaz task            # Run task commands once and exit
zaz daemon          # Run the daemon in the foreground
zaz status          # Show daemon status
zaz restart [group] # Restart a group (or all)
zaz stop            # Stop the running daemon
zaz ignores         # Show default ignore patterns
```

## TUI Options

```bash
zaz --full          # Full style: split panes with group tree + logs
zaz --multi-pane    # Multi-pane style: one pane per task
zaz --no-autostart  # Don't auto-start a daemon before opening the TUI
zaz --stop-on-exit  # Stop the connected daemon when the TUI exits
```

Press `F1`/`F2` to switch between Full and Multi Pane styles at runtime.

## User Configuration

User preferences are stored separately from project configuration at
`~/.config/zaz/config.toml` (following XDG Base Directory specification):

```toml
# Don't auto-start a daemon before opening the TUI
no_autostart = false

# Disable blinking/animation effects
disable_animations = false

# Default TUI style: "full" or "multi_pane"
tui_style = "full"
```

These settings are optional - zaz works fine without a user config file.
CLI flags take precedence over user config values. The legacy value
`"minimal"` is still accepted as an alias for `"multi_pane"`.

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
| `F2` | Switch to Multi Pane style |
| `[`/`]` | Previous/next page (Multi Pane, >6 tasks) |

### General

| Key | Action |
|-----|--------|
| `q` | Quit |
| `?` | Toggle help overlay |

## License

MIT
