# Node/TypeScript dev server

Run `tsc --noEmit` on save and keep `vite` running as a service. The shared
port lives in `[variables]` so the service command and any future task can
reference it without hand-syncing.

## Features used

- `[settings] debounce` to widen the file-change batching window for editors
  that save on every keystroke.
- `[variables]` for shared values referenced via `${port}`.
- A single group holding one task and one service.
- Default `signal = "SIGTERM"` and PTY enabled (vite expects a TTY for its
  pretty output).

## Try it

```sh
zaz check
zaz                           # default TUI mode
zaz daemon                    # foreground daemon
zaz restart web vite          # restart just the vite service
```

## See also

- [../../configuration.md](../../configuration.md) for the full schema and
  the `${...}` expansion rules.
- [../../tui.md](../../tui.md) for keyboard shortcuts while watching.
