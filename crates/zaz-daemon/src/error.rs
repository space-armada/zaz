//! Error types for the daemon.

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur in the daemon.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// Failed to bind to socket.
    #[error("failed to bind to socket: {0}")]
    Bind(#[from] std::io::Error),

    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(#[from] zaz_config::ConfigError),

    /// Watch error.
    #[error("watch error: {0}")]
    Watch(#[from] zaz_watch::WatchError),

    /// Process error.
    #[error("process error: {0}")]
    Process(#[from] zaz_process::ProcessError),

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Group not found.
    #[error("group not found: {0}")]
    GroupNotFound(String),

    /// Cyclic dependency detected.
    #[error("cyclic dependency detected involving: {0}")]
    CyclicDependency(String),

    /// Variable expansion error.
    #[error("variable expansion error: {0}")]
    VarExpansion(String),

    /// Task failed.
    #[error("task '{task}' failed: {error}")]
    TaskFailed { task: String, error: String },

    /// Socket target could not be resolved from the current location.
    #[error("could not resolve daemon socket from {}", start_dir.display())]
    SocketResolution { start_dir: PathBuf },

    /// Workspace socket target could not be resolved from the current location.
    #[error("could not resolve workspace socket from {}", start_dir.display())]
    WorkspaceSocketResolution { start_dir: PathBuf },

    /// Log storage backend reported an error.
    #[error("log storage error: {0}")]
    LogStorage(#[from] LogStorageError),
}

/// Errors that can occur in a log storage backend.
///
/// The in-memory backend currently never returns these, but every variant maps
/// to a failure mode that the persistent SQLite backend must be able to surface
/// without silently dropping API log history.
#[derive(Debug, Error)]
pub enum LogStorageError {
    /// Failed to open the backing store.
    #[error("failed to open log storage: {0}")]
    Open(String),

    /// Failed to initialize the storage schema.
    #[error("failed to initialize log storage schema: {0}")]
    SchemaInit(String),

    /// Write to the backing store failed.
    #[error("failed to write logs: {0}")]
    Write(String),

    /// Query against the backing store failed.
    #[error("failed to query logs: {0}")]
    Query(String),

    /// Acquiring an exclusive write lock timed out.
    #[error("log storage lock timed out")]
    LockTimeout,

    /// Retention sweep failed.
    #[error("failed to enforce log retention: {0}")]
    Retention(String),
}

impl DaemonError {
    /// Recovery suggestion for variants where a concrete next step exists.
    /// Returns `None` for terminal errors with no actionable recovery.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::SocketResolution { .. } => {
                Some("run this command from a zaz project directory or pass --socket <PATH>")
            }
            Self::WorkspaceSocketResolution { .. } => {
                Some("create a .zaz/ workspace root above your configs or pass --socket <PATH>")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_resolution_display_omits_recovery_prose() {
        let err = DaemonError::SocketResolution {
            start_dir: PathBuf::from("/tmp/outside"),
        };
        let msg = err.to_string();
        assert!(msg.contains("could not resolve daemon socket from"));
        assert!(msg.contains("/tmp/outside"));
        assert!(!msg.contains("--socket"));
        assert!(!msg.contains("zaz project directory"));
    }

    #[test]
    fn socket_resolution_hint_returns_recovery_prose() {
        let err = DaemonError::SocketResolution {
            start_dir: PathBuf::from("/tmp/outside"),
        };
        assert_eq!(
            err.hint(),
            Some("run this command from a zaz project directory or pass --socket <PATH>")
        );
    }

    #[test]
    fn workspace_socket_resolution_display_omits_recovery_prose() {
        let err = DaemonError::WorkspaceSocketResolution {
            start_dir: PathBuf::from("/tmp/outside"),
        };
        let msg = err.to_string();
        assert!(msg.contains("could not resolve workspace socket from"));
        assert!(msg.contains("/tmp/outside"));
        assert!(!msg.contains("--socket"));
        assert!(!msg.contains(".zaz/"));
    }

    #[test]
    fn workspace_socket_resolution_hint_returns_recovery_prose() {
        let err = DaemonError::WorkspaceSocketResolution {
            start_dir: PathBuf::from("/tmp/outside"),
        };
        assert_eq!(
            err.hint(),
            Some("create a .zaz/ workspace root above your configs or pass --socket <PATH>")
        );
    }

    #[test]
    fn other_variants_have_no_hint() {
        assert_eq!(DaemonError::GroupNotFound("x".into()).hint(), None);
        assert_eq!(DaemonError::CyclicDependency("y".into()).hint(), None);
    }

    #[test]
    fn log_storage_error_wraps_into_daemon_error() {
        let err: DaemonError = LogStorageError::Write("disk full".into()).into();
        assert_eq!(
            err.to_string(),
            "log storage error: failed to write logs: disk full"
        );
    }
}
