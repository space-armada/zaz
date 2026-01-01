//! Configuration parsing for zaz.
//!
//! Supports both TOML and JSON formats, auto-detected by file extension.

mod error;
mod schema;

pub use error::ConfigError;
pub use schema::{Config, DaemonCommand, Group, LogFormat, PrepCommand, Settings, Signal};

use std::path::Path;

/// Load configuration from a file, auto-detecting format by extension.
pub fn load<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => parse_toml(&contents),
        Some("json") => parse_json(&contents),
        _ => Err(ConfigError::UnknownFormat {
            path: path.to_path_buf(),
        }),
    }
}

/// Parse TOML configuration.
pub fn parse_toml(contents: &str) -> Result<Config, ConfigError> {
    toml::from_str(contents).map_err(|e| ConfigError::Toml(e.to_string()))
}

/// Parse JSON configuration.
pub fn parse_json(contents: &str) -> Result<Config, ConfigError> {
    serde_json::from_str(contents).map_err(ConfigError::Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_toml() {
        let config = parse_toml("").unwrap();
        assert!(config.groups.is_empty());
    }

    #[test]
    fn test_parse_minimal_json() {
        let config = parse_json("{}").unwrap();
        assert!(config.groups.is_empty());
    }
}
