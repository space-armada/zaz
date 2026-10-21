//! Error types for the daemon.

use crate::state::LoadError;
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur in the daemon.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// Failed to bind to socket.
    #[error("failed to bind to socket: {0}")]
    Bind(#[from] std::io::Error),

    /// Configuration error.
    #[error("configuration error: {}", format_config_error(.0))]
    Config(#[from] zaz_config::ConfigError),

    /// One or more `env_file`s could not be loaded.
    #[error("{}", format_env_file_errors(.0))]
    EnvFile(Vec<zaz_config::EnvFileError>),

    /// The config and the sources it names did not finish loading within the limit.
    ///
    /// The thread doing the read cannot be stopped, so it is abandoned.
    #[error(
        "a source did not finish reading within {:?}, so the load was abandoned [env_load_timeout]",
        .limit
    )]
    LoadTimeout { limit: std::time::Duration },

    /// The thread that loads and evaluates a config panicked.
    #[error("loading the config panicked")]
    EvaluationPanicked,

    /// The thread that loads and evaluates a config could not start.
    #[error("could not start the thread that loads the config: {0}")]
    EvaluationThread(std::io::Error),

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

/// A validation failure with one line per error, each ending in its code. Any other config error
/// keeps its own message.
fn format_config_error(error: &zaz_config::ConfigError) -> String {
    match error {
        zaz_config::ConfigError::Validation(errors) => errors.display_with_codes(),
        other => other.to_string(),
    }
}

/// One line per error, each ending in its code.
fn format_env_file_errors(errors: &[zaz_config::EnvFileError]) -> String {
    errors
        .iter()
        .map(|e| format!("{e} [{}]", e.code()))
        .collect::<Vec<_>>()
        .join("\n")
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
    /// A stable code for the failure. A load failure reports the code of its first error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EnvFile(errors) => errors.first().map_or("env_file_error", |e| e.code()),
            Self::Config(error) => error.code(),
            Self::LoadTimeout { .. } => "env_load_timeout",
            Self::EvaluationPanicked => "evaluation_panicked",
            Self::EvaluationThread(_) => "evaluation_unavailable",
            Self::CyclicDependency(_) => "dependency_cycle",
            Self::Watch(_) => "watch_error",
            Self::LogStorage(_) => "log_storage_error",
            _ => "load_failed",
        }
    }

    /// This failure in the form a daemon reports as its `load_error`.
    pub fn to_load_error(&self) -> LoadError {
        let error = LoadError::new(self.code(), self.to_string());

        match self.hint() {
            Some(hint) => error.with_hint(hint),
            None => error,
        }
    }

    /// Recovery suggestion for variants where a concrete next step exists.
    /// Returns `None` for terminal errors with no actionable recovery.
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::EnvFile(errors) => errors.first().and_then(|e| e.hint()).map(str::to_string),
            Self::Config(zaz_config::ConfigError::Validation(errors)) => {
                errors.iter().next().and_then(|e| e.hint.clone())
            }
            Self::LoadTimeout { .. } => Some(
                "a named pipe or other special file in an env_file may have no writer, so \
                 point env_file at a regular file"
                    .to_string(),
            ),
            Self::SocketResolution { .. } => Some(
                "run this command from a zaz project directory or pass --socket <PATH>".to_string(),
            ),
            Self::WorkspaceSocketResolution { .. } => Some(
                "create a .zaz/ workspace root above your configs or pass --socket <PATH>"
                    .to_string(),
            ),
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
            err.hint().as_deref(),
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
            err.hint().as_deref(),
            Some("create a .zaz/ workspace root above your configs or pass --socket <PATH>")
        );
    }

    #[test]
    fn a_validation_failure_carries_a_code_on_every_line_and_the_first_hint() {
        let toml =
            "[[group]]\nname = \"api\"\npatterns = [\"*\"]\nenv_flie = \".env\"\nbogus = 1\n";
        let error = DaemonError::from(zaz_config::parse_toml(toml).unwrap_err());

        let message = error.to_string();
        assert_eq!(message.matches("[unknown_key]").count(), 2, "{message}");
        assert_eq!(error.code(), "unknown_key");
        assert_eq!(error.hint().as_deref(), Some("did you mean 'env_file'?"));
        assert_eq!(
            error.to_load_error().hint.as_deref(),
            Some("did you mean 'env_file'?")
        );
    }

    #[test]
    fn a_non_validation_config_failure_uses_the_shared_code() {
        let error = DaemonError::from(zaz_config::parse_toml("invalid = [").unwrap_err());

        assert_eq!(error.code(), "parse_error");
    }

    #[test]
    fn a_load_timeout_has_its_own_code_and_says_what_stalled() {
        let error = DaemonError::LoadTimeout {
            limit: std::time::Duration::from_secs(120),
        };

        assert_eq!(error.code(), "env_load_timeout");
        assert!(error
            .to_string()
            .contains("did not finish reading within 120s"));
        assert!(error.hint().is_some());
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
