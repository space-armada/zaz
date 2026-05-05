# TUI reference

Reference for the zaz TUI: styles, modes, and keyboard shortcuts. The TUI is
launched by the bare `zaz` invocation; for the launch flags `--full`,
`--multi-pane`, `--no-autostart`, and `--stop-on-exit`, see
[cli.md](cli.md#zaz-default-tui-mode).

## Overview

The TUI connects to a running daemon over the resolved Unix socket and shows
group, task, and daemon state plus per-process logs. It autostarts a daemon
when one is not already running unless `--no-autostart` is passed. The
default style and other operator preferences are read from the user config
documented in [user-configuration.md](user-configuration.md); CLI flags
override those preferences.

## Styles

Two styles ship today:

| Style | Layout | Best for |
|-------|--------|----------|
| Full | Two panes — group tree on the left, combined logs on the right | Browsing groups, reading interleaved logs across all processes |
| Multi-Pane | A grid of per-process panes | Watching a small number of processes side by side |

`F1` switches to Full at runtime; `F2` switches to Multi-Pane. The TUI
remembers focus state per style; switching does not lose your filter or
search.

The two styles differ in their focus model:

- **Full** has two focus targets, *Groups* and *Logs*. `Tab` toggles between
  them. `j`/`k` and the arrow keys navigate the focused pane.
- **Multi-Pane** focuses one pane at a time. `Tab` cycles forward, `Shift+Tab`
  cycles backward, and arrow keys (or `h`/`l`) move within the grid. `j`/`k`
  scroll the focused pane's logs by a single line.

## Keyboard shortcuts

Style-specific bindings are marked **Full** or **Multi-Pane**; rows without
a marker apply to both.

### Navigation

| Key | Action | Style |
|-----|--------|-------|
| `j`, `↓` | Move down (group tree when focused; otherwise scroll one line) | Full |
| `k`, `↑` | Move up (group tree when focused; otherwise scroll one line) | Full |
| `j` | Scroll focused pane down one line | Multi-Pane |
| `k` | Scroll focused pane up one line | Multi-Pane |
| `↓` / `↑` | Move to the pane below / above in the grid | Multi-Pane |
| `h`, `←` | Move to the pane on the left | Multi-Pane |
| `l`, `→` | Move to the pane on the right | Multi-Pane |
| `Tab` | Toggle focus between Groups and Logs | Full |
| `Tab` | Cycle to the next pane | Multi-Pane |
| `Shift+Tab` | Cycle to the previous pane | Multi-Pane |
| `g` | Jump to the top of the focused logs / pane | |
| `G` | Jump to the bottom and re-enable follow mode | |
| `Page Up` | Scroll the focused logs / pane up by one screen | |
| `Page Down` | Scroll the focused logs / pane down by one screen | |
| `Ctrl+u` | Scroll up by half a screen | |
| `Ctrl+d` | Scroll down by half a screen | |

Manual scrolling disables follow mode for the affected logs / pane until
`G` or `F` re-enables it.

### Actions

| Key | Action |
|-----|--------|
| `r` | Restart the selected group, task, or daemon |
| `R` | Restart all groups |
| `c` | Clear the focused logs / pane (client-local view cutoff; daemon retains the full log) |
| `F` | Toggle follow mode for the focused logs / pane |
| `t` | Toggle between compact and full timestamps |

`R` is `Shift+r`; the lowercase `r` only restarts the current selection.
Clearing logs is a per-client view cutoff per phase 7 / phase 12 — the
daemon's stored log is unchanged, and switching processes or relaunching
the TUI shows previously cleared lines again.

### Search and filter

Filter and search both accept Rust regular expressions over rendered log
content. Patterns are case-sensitive by default; prefix with `(?i)` for
case-insensitive matching. Invalid regexes do not enter filter / search
state; the TUI sets a status-bar error instead.

| Key | Action |
|-----|--------|
| `f` | Enter filter mode |
| `/` | Enter search mode |
| `Enter` | Apply the typed regex (filter or search) |
| `Backspace` | Delete the last character of the input |
| `Esc` | Cancel input and discard the typed regex |
| `n` | Jump to the next search match |
| `N` | Jump to the previous search match |

While a filter is active, lazy log loading from the daemon is disabled; only
already-loaded lines are matched. Clearing the filter (see Esc table below)
re-enables lazy loading.

### Style and pane controls

| Key | Action | Style |
|-----|--------|-------|
| `F1` | Switch to Full | |
| `F2` | Switch to Multi-Pane | |
| `[` | Previous page of panes | Multi-Pane |
| `]` | Next page of panes | Multi-Pane |
| `+`, `=` | Increase panes per page (max 6) | Multi-Pane |
| `-`, `_` | Decrease panes per page (min 1) | Multi-Pane |

### General

| Key | Action |
|-----|--------|
| `q` | Quit the TUI |
| `Ctrl+C` | Quit the TUI (alternative) |
| `?` | Open the help overlay |

`q` only quits when no modifier is held; `Shift+q` (uppercase `Q`) is
ignored. With `--stop-on-exit`, quitting also stops the connected daemon.

## Esc behavior

`Esc` is overloaded across modes. The exact effect depends on what is
showing:

| When | Effect |
|------|--------|
| Help overlay is open | Close the overlay |
| Filter input is active (`f` mode) | Cancel input; return to Normal mode without applying |
| Search input is active (`/` mode) | Cancel input; return to Normal mode without applying |
| Normal mode with an applied filter or search | Clear both the filter and the search; reset status |

Clearing in Normal mode does not exit the TUI; only `q` and `Ctrl+C` do
that.

## Help overlay

`?` opens a modal help overlay. While it is showing, every other key is
ignored except `?` itself and `Esc`, which both close the overlay. The
overlay's content groups bindings into the same five categories as this
page: Navigation, Actions, Search & Filter, Style, and General.
