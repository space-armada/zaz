//! Error types for the TUI.

use thiserror::Error;

/// Errors that can occur in the TUI.
#[derive(Debug, Error)]
pub enum TuiError {
    /// Terminal I/O error.
    #[error("terminal error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to connect to daemon.
    #[error("failed to connect to daemon: {0}")]
    Connection(String),
}
