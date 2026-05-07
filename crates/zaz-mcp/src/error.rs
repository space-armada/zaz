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
    #[error("daemon is not running at {}", socket.display())]
    DaemonNotRunning { socket: PathBuf },

    /// Could not locate a zaz project from the current working directory.
    #[error("no zaz config found at or above {}", start_dir.display())]
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

impl McpError {
    /// Recovery suggestion for variants where a concrete next step exists.
    /// Returns `None` for terminal errors with no actionable recovery.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::DaemonNotRunning { .. } => Some("start it with `zaz` or `zaz start`"),
            Self::NoConfig { .. } => {
                Some("run `zaz mcp` from a project directory containing zaz.toml or zaz.json")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_not_running_display_omits_recovery_prose() {
        let err = McpError::DaemonNotRunning {
            socket: PathBuf::from("/tmp/zaz.sock"),
        };
        let msg = err.to_string();
        assert!(msg.contains("daemon is not running at"));
        assert!(msg.contains("/tmp/zaz.sock"));
        assert!(!msg.contains("zaz start"));
    }

    #[test]
    fn daemon_not_running_hint_returns_recovery_prose() {
        let err = McpError::DaemonNotRunning {
            socket: PathBuf::from("/tmp/zaz.sock"),
        };
        assert_eq!(err.hint(), Some("start it with `zaz` or `zaz start`"));
    }

    #[test]
    fn no_config_display_omits_recovery_prose() {
        let err = McpError::NoConfig {
            start_dir: PathBuf::from("/tmp/elsewhere"),
        };
        let msg = err.to_string();
        assert!(msg.contains("no zaz config found at or above"));
        assert!(msg.contains("/tmp/elsewhere"));
        assert!(!msg.contains("zaz mcp"));
        assert!(!msg.contains("zaz.toml"));
    }

    #[test]
    fn no_config_hint_returns_recovery_prose() {
        let err = McpError::NoConfig {
            start_dir: PathBuf::from("/tmp/elsewhere"),
        };
        assert_eq!(
            err.hint(),
            Some("run `zaz mcp` from a project directory containing zaz.toml or zaz.json")
        );
    }

    #[test]
    fn other_variants_have_no_hint() {
        assert_eq!(McpError::Serve("x".into()).hint(), None);
        assert_eq!(McpError::UnexpectedResponse("y".into()).hint(), None);
        assert_eq!(
            McpError::DaemonRefused {
                operation: "restart_group",
                message: "unknown".into(),
            }
            .hint(),
            None
        );
    }
}
