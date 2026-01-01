//! Error types for configuration parsing.

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur when loading or parsing configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read configuration file.
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Unknown configuration file format.
    #[error("unknown config format for {path} (expected .toml or .json)")]
    UnknownFormat { path: PathBuf },

    /// TOML parsing error.
    #[error("TOML parse error: {0}")]
    Toml(String),

    /// JSON parsing error.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// Validation error.
    #[error("validation error: {0}")]
    Validation(String),
}
