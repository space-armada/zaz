//! Error types for configuration parsing.

use std::ops::Range;
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur when loading or parsing configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// No configuration file found.
    #[error("no configuration file found, searched: {}", format_paths(.searched))]
    NotFound { searched: Vec<PathBuf> },

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
    #[error("TOML parse error: {message}{}", format_span(.span))]
    Toml {
        message: String,
        span: Option<Range<usize>>,
    },

    /// JSON parsing error.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// Validation error.
    #[error("configuration validation failed:\n{0}")]
    Validation(String),
}

fn format_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_span(span: &Option<Range<usize>>) -> String {
    match span {
        Some(range) => format!(" at bytes {}..{}", range.start, range.end),
        None => String::new(),
    }
}

impl ConfigError {
    /// Check if this is a "not found" error.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ConfigError::NotFound { .. })
    }

    /// Check if this is a validation error.
    pub fn is_validation(&self) -> bool {
        matches!(self, ConfigError::Validation(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_found_error_display() {
        let err = ConfigError::NotFound {
            searched: vec![PathBuf::from("zaz.toml"), PathBuf::from("zaz.json")],
        };
        let msg = err.to_string();
        assert!(msg.contains("zaz.toml"));
        assert!(msg.contains("zaz.json"));
    }

    #[test]
    fn test_toml_error_with_span() {
        let err = ConfigError::Toml {
            message: "expected value".to_string(),
            span: Some(10..15),
        };
        let msg = err.to_string();
        assert!(msg.contains("expected value"));
        assert!(msg.contains("bytes 10..15"));
    }

    #[test]
    fn test_validation_error() {
        let err = ConfigError::Validation("group 'foo': duplicate name".to_string());
        assert!(err.is_validation());
        assert!(err.to_string().contains("duplicate name"));
    }
}
