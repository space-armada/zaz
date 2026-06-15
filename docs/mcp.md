# zaz MCP tool server

`zaz mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
tool server over stdio. It is a thin client of the daemon's Unix socket API,
so an AI assistant configured with the server can ask the running daemon for
state, read process logs, and trigger restarts without the operator having
to switch to a terminal and copy output back.

## Running the server

```bash
zaz mcp                          # talk to the daemon resolved from CWD
zaz mcp --autostart              # spawn a background daemon if none is running
zaz mcp --socket /path/to.sock   # explicit socket override
zaz mcp --config ./zaz.toml      # explicit config override (derives the socket)
```

The daemon must already be running, or `--autostart` must be passed; otherwise
tool calls return an actionable error telling the user to start one. The
global `--debug`, `--log-file`, `--socket`, and `--config` flags work the same
way as on every other `zaz` subcommand.

The MCP server uses stdout as its JSON-RPC channel, so logs are always
written to stderr or to `--log-file`. Never redirect stdout when invoking
`zaz mcp` from a client.

## Tools

| Tool | Args | Purpose |
|------|------|---------|
| `zaz_status` | (none) | Daemon state, all groups and processes (pid, exit code, duration). |
| `zaz_list_groups` | (none) | Slim group listing: name, status, task and daemon counts. |
| `zaz_logs` | `name?`, `offset?`, `limit?`, `search?` | Paginated log lines for one process, or `*` for all. |
| `zaz_config` | (none) | Parsed config: settings, variables, groups, tasks, daemons. |
| `zaz_restart_group` | `name` | Restart every process in one group. |
| `zaz_restart_process` | `group`, `process` | Restart a single task or daemon. |
| `zaz_restart_all` | (none) | Restart every group, respecting `depends_on`. |
| `zaz_reload_config` | (none) | Re-read `zaz.toml`/`zaz.json` from disk and apply diffs. |

`zaz_shutdown` is intentionally not exposed: shutting down the process
manager is qualitatively different from the reversible operations above and
has a weak agent use case.

When the daemon refuses an operation — unknown group name, parse error on
reload, and so on — its error message is returned verbatim so the agent can
surface it to the user without paraphrase.

`zaz_logs` consumes persistent history when the daemon runs with the
SQLite backend (`backend = "sqlite"` in user config), so pre-restart
lines remain queryable after the daemon has been restarted. Under the
default `memory` backend the same call serves whatever is in the
in-memory buffer at the moment of the request. The request shape
(`name`, `offset`, `limit`, `search`) and the response fields are
identical across both modes. See
[user-configuration.md#log_storage](user-configuration.md#log_storage)
for backend selection and the retention contract.

## Client configuration

### Claude Code

Add an entry to `.mcp.json` at the project root (project-scoped) or to
`~/.claude.json` (user-scoped):

```json
{
  "mcpServers": {
    "zaz": {
      "command": "zaz",
      "args": ["mcp"]
    }
  }
}
```

Or use the CLI form, which writes the same shape:

```bash
claude mcp add zaz -- zaz mcp
```

To autostart a daemon on first tool call, change `args` to
`["mcp", "--autostart"]`.

### Cursor

Add the same `mcpServers` block to `.cursor/mcp.json` at the project root or
`~/.cursor/mcp.json` for a global registration:

```json
{
  "mcpServers": {
    "zaz": {
      "command": "zaz",
      "args": ["mcp"]
    }
  }
}
```

### Generic stdio clients

`zaz mcp` reads JSON-RPC requests from stdin and writes responses to stdout,
following the MCP stdio transport. Any client that can spawn a child process
and exchange newline-delimited JSON-RPC frames works: spawn `zaz mcp`, send
the standard `initialize` handshake, then call tools by name.

## Socket discovery and overrides

`zaz mcp` resolves the daemon socket the same way every other zaz command
does: it walks up from the spawned process's working directory looking for
a `zaz.toml` or `zaz.json`, then derives the socket path from that config.
Most MCP clients spawn servers with the project root as cwd, so a single
`.mcp.json` at the project root just works.

When that is not desirable — for example a globally registered server that
needs to target a specific project — pass `--socket` or `--config` in
`args`:

```json
{
  "mcpServers": {
    "zaz-backend": {
      "command": "zaz",
      "args": ["mcp", "--config", "/abs/path/to/backend/zaz.toml"]
    }
  }
}
```

`--socket` wins over `--config` when both are passed.

### Workspace supervisor

A workspace covers the handful of zaz projects you are touching in a monorepo
under one registration. Start it with two or more `--config` flags:

```sh
zaz start -c backend/zaz.toml -c frontend/zaz.toml
```

This launches a supervisor that owns one child daemon per member and binds a
single workspace socket at `<workspace-root>/.zaz/daemon.sock`, where the
workspace root is the nearest ancestor directory holding a `.zaz/` directory
but no `zaz.toml` or `zaz.json`. Pin that socket once and a single MCP
registration reaches the whole set:

```json
{
  "mcpServers": {
    "zaz-workspace": {
      "command": "zaz",
      "args": ["mcp", "--socket", "/abs/path/to/workspace/.zaz/daemon.sock"]
    }
  }
}
```

`zaz_status` against the supervisor merges every member's groups under
`project/group` keys. `zaz_logs`, `zaz_restart_group`, and `zaz_restart_process`
take a structured `project` parameter to target one member; a log query without
a `project` is rejected rather than fanned out, since each member keeps its own
log database.
