# TUI reference

Reference for the zaz TUI: styles, modes, and keyboard shortcuts. The TUI is
launched via the default `zaz` invocation; for TUI launch flags
(`--full`, `--multi-pane`, `--no-autostart`, `--stop-on-exit`), see
[cli.md](cli.md#zaz-default-tui-mode).

This file is a stub; sections are populated in milestone 23.5.

## Overview

To be written in milestone 23.5.

## Styles

Two styles ship today: **Full** (split panes with group tree plus logs) and
**Multi-Pane** (one pane per task). `F1` and `F2` switch between them at
runtime.

To be written in milestone 23.5.

## Keyboard shortcuts

### Navigation

To be written in milestone 23.5.

### Actions

To be written in milestone 23.5.

### Search and filter

Filter (`f`) and search (`/`) both accept regular expressions over log
content. `Esc` cancels filter or search input and returns to the last applied
state.

To be written in milestone 23.5.

### Style and pane controls

`F1` and `F2` switch styles. In Multi-Pane, `+`/`=` and `-`/`_` adjust
panes-per-page; `Tab` and `Shift+Tab` cycle the focused pane forward and
backward.

To be written in milestone 23.5.

### General

`q` and `Ctrl+C` quit. `?` opens the help overlay; `?` or `Esc` closes it.

To be written in milestone 23.5.

## Help overlay

To be written in milestone 23.5.
