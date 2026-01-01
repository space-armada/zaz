//! Error types for process management.

use std::io;
use thiserror::Error;

/// Errors that can occur during process management.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// Failed to spawn process.
    #[error("failed to spawn process: {0}")]
    Spawn(#[from] io::Error),

    /// Failed to allocate PTY.
    #[error("failed to allocate PTY: {0}")]
    Pty(String),

    /// Process exited with non-zero status.
    #[error("process exited with status {0}")]
    ExitStatus(i32),

    /// Process was killed by signal.
    #[error("process killed by signal {0}")]
    Signal(i32),

    /// Failed to send signal to process.
    #[error("failed to send signal: {0}")]
    SendSignal(#[from] nix::Error),
}
