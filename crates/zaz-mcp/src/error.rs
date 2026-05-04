//! Error types for the MCP server.

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur while running the MCP server or servicing tools.
#[derive(Debug, Error)]
pub enum McpError {
    /// I/O failure on the stdio transport or the daemon socket connection.
    #[error("mcp transport error: {0}")]
    Io(#[from] std::io::Error),

    /// Failure raised by the rmcp service layer (initialize, dispatch, shutdown).
    #[error("mcp service error: {0}")]
    Serve(String),

    /// The daemon socket exists in resolution but no process is listening on it.
    #[error(
        "daemon is not running at {}: start it with `zaz` or `zaz start`",
        socket.display()
    )]
    DaemonNotRunning { socket: PathBuf },

    /// Could not locate a zaz project from the current working directory.
    #[error(
        "no zaz config found at or above {}: run `zaz mcp` from a project directory containing zaz.toml or zaz.json",
        start_dir.display()
    )]
    NoConfig { start_dir: PathBuf },

    /// Underlying daemon library error (socket resolution, serialization, etc.).
    #[error("daemon error: {0}")]
    Daemon(#[from] zaz_daemon::DaemonError),

    /// Configuration loading or validation error.
    #[error("config error: {0}")]
    Config(#[from] zaz_config::ConfigError),

    /// The daemon returned a response shape the client did not expect.
    #[error("unexpected daemon response: {0}")]
    UnexpectedResponse(String),

    /// The daemon understood the request but refused to perform the operation,
    /// e.g. an unknown group name or a config reload that fails to parse.
    #[error("daemon refused {operation}: {message}")]
    DaemonRefused {
        operation: &'static str,
        message: String,
    },
}
