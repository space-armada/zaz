//! Configuration parsing for zaz.
//!
//! Supports both TOML and JSON formats, auto-detected by file extension.

mod error;
mod schema;
pub mod user;
mod validate;

pub use error::ConfigError;
pub use schema::{Config, DaemonCommand, Group, LogFormat, Settings, Signal, Silence, TaskCommand};
pub use user::{
    load_user_config, user_config_path, ColorRule, LogColorConfig, NotificationConfig,
    TuiStylePreference, UserConfig,
};
pub use validate::validate;

use std::path::{Path, PathBuf};

/// Default configuration file names to search for.
pub const CONFIG_FILES: &[&str] = &["zaz.toml", "zaz.json"];

/// Discover and load a configuration file from the current directory.
///
/// Searches for `zaz.toml` first, then `zaz.json`.
pub fn discover() -> Result<(PathBuf, Config), ConfigError> {
    discover_in(std::env::current_dir().map_err(|e| ConfigError::Io {
        path: PathBuf::from("."),
        source: e,
    })?)
}

/// Discover and load a configuration file from the given directory.
pub fn discover_in<P: AsRef<Path>>(dir: P) -> Result<(PathBuf, Config), ConfigError> {
    let dir = dir.as_ref();

    for filename in CONFIG_FILES {
        let path = dir.join(filename);
        if path.exists() {
            let config = load(&path)?;
            return Ok((path, config));
        }
    }

    Err(ConfigError::NotFound {
        searched: CONFIG_FILES.iter().map(|s| dir.join(s)).collect(),
    })
}

/// Load configuration from a file, auto-detecting format by extension.
pub fn load<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let config = match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => parse_toml(&contents)?,
        Some("json") => parse_json(&contents)?,
        _ => {
            return Err(ConfigError::UnknownFormat {
                path: path.to_path_buf(),
            })
        }
    };

    // Validate the config
    validate(&config)?;

    Ok(config)
}

/// Load configuration without validation (for testing).
pub fn load_unvalidated<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
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
    toml::from_str(contents).map_err(|e| ConfigError::Toml {
        message: e.message().to_string(),
        span: e.span(),
    })
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

    #[test]
    fn test_parse_full_toml() {
        let toml = r#"
[settings]
shell = "bash"
debounce_ms = 200
log_format = "json"

[variables]
build_dir = "./build"

[[group]]
name = "backend"
patterns = ["**/*.go"]
ignore = ["**/vendor/**"]

[[group.task]]
name = "test"
command = "go test ./..."

[[group.daemon]]
name = "server"
command = "./server"
signal = "SIGTERM"
no_pty = false
"#;
        let config = parse_toml(toml).unwrap();
        assert_eq!(config.settings.shell, Some("bash".to_string()));
        assert_eq!(config.settings.debounce_ms, 200);
        assert_eq!(config.settings.log_format, LogFormat::Json);
        assert_eq!(
            config.variables.get("build_dir"),
            Some(&"./build".to_string())
        );
        assert_eq!(config.groups.len(), 1);
        assert_eq!(config.groups[0].name, "backend");
        assert_eq!(config.groups[0].tasks.len(), 1);
        assert_eq!(config.groups[0].daemons.len(), 1);
    }

    #[test]
    fn test_parse_full_json() {
        let json = r#"{
            "settings": {
                "shell": "bash",
                "debounce_ms": 200,
                "log_format": "json"
            },
            "variables": {
                "build_dir": "./build"
            },
            "groups": [{
                "name": "backend",
                "patterns": ["**/*.go"],
                "ignore": ["**/vendor/**"],
                "tasks": [{
                    "name": "test",
                    "command": "go test ./..."
                }],
                "daemons": [{
                    "name": "server",
                    "command": "./server",
                    "signal": "SIGTERM",
                    "no_pty": false
                }]
            }]
        }"#;
        let config = parse_json(json).unwrap();
        assert_eq!(config.settings.shell, Some("bash".to_string()));
        assert_eq!(config.groups.len(), 1);
        assert_eq!(config.groups[0].name, "backend");
    }

    #[test]
    fn test_toml_group_alias() {
        // Test that [[group]] works (the alias for groups)
        let toml = r#"
[[group]]
name = "test"
patterns = ["*.txt"]
"#;
        let config = parse_toml(toml).unwrap();
        assert_eq!(config.groups.len(), 1);
    }

    #[test]
    fn test_json_daemon_alias() {
        // Test that "daemon" works as alias for "daemons"
        let json = r#"{
            "groups": [{
                "name": "test",
                "patterns": ["*.txt"],
                "daemon": [{
                    "name": "srv",
                    "command": "./srv"
                }]
            }]
        }"#;
        let config = parse_json(json).unwrap();
        assert_eq!(config.groups[0].daemons.len(), 1);
    }

    #[test]
    fn test_default_values() {
        let config = parse_toml("").unwrap();
        assert_eq!(config.settings.debounce_ms, 100);
        assert_eq!(config.settings.log_format, LogFormat::Pretty);
        assert!(config.settings.shell.is_none());
    }

    #[test]
    fn test_signal_parsing() {
        let toml = r#"
[[group]]
name = "test"
patterns = ["*.txt"]

[[group.daemon]]
name = "srv"
command = "./srv"
signal = "SIGHUP"
"#;
        let config = parse_toml(toml).unwrap();
        assert_eq!(config.groups[0].daemons[0].signal, Signal::Sighup);
    }

    #[test]
    fn test_depends_on() {
        let toml = r#"
[[group]]
name = "backend"
patterns = ["**/*.go"]

[[group]]
name = "frontend"
patterns = ["**/*.ts"]
depends_on = ["backend"]
"#;
        let config = parse_toml(toml).unwrap();
        assert_eq!(config.groups[1].depends_on, vec!["backend"]);
    }

    #[test]
    fn test_invalid_toml() {
        let result = parse_toml("invalid = [");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Toml { .. }));
    }

    #[test]
    fn test_invalid_json() {
        let result = parse_json("{invalid}");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Json(_)));
    }

    #[test]
    fn test_silence_parsing() {
        use crate::Silence;

        let toml = r#"
[[group]]
name = "test"
patterns = ["*.txt"]

[[group.task]]
name = "quiet"
command = "echo hello"
silence = "all"

[[group.task]]
name = "stdout_only"
command = "echo hello"
silence = "stderr"

[[group.daemon]]
name = "srv"
command = "./srv"
silence = "stdout"
"#;
        let config = parse_toml(toml).unwrap();

        // Task with silence = "all"
        assert_eq!(config.groups[0].tasks[0].silence, Silence::All);
        // Task with silence = "stderr" (shows only stderr)
        assert_eq!(config.groups[0].tasks[1].silence, Silence::Stderr);
        // Daemon with silence = "stdout"
        assert_eq!(config.groups[0].daemons[0].silence, Silence::Stdout);
    }

    #[test]
    fn test_silence_default() {
        use crate::Silence;

        let toml = r#"
[[group]]
name = "test"
patterns = ["*.txt"]

[[group.task]]
name = "default"
command = "echo hello"
"#;
        let config = parse_toml(toml).unwrap();
        // Default should be None
        assert_eq!(config.groups[0].tasks[0].silence, Silence::None);
    }

    #[test]
    fn test_working_dir_parsing() {
        let toml = r#"
[[group]]
name = "monorepo"
patterns = ["**/*.ts"]
working_dir = "./packages"

[[group.task]]
name = "frontend-lint"
command = "npm run lint"
working_dir = "./packages/frontend"

[[group.task]]
name = "backend-lint"
command = "npm run lint"
working_dir = "./packages/backend"

[[group.daemon]]
name = "server"
command = "./server"
working_dir = "./packages/server"
"#;
        let config = parse_toml(toml).unwrap();

        // Group working_dir
        assert_eq!(config.groups[0].working_dir, Some("./packages".to_string()));

        // Task working_dir overrides
        assert_eq!(
            config.groups[0].tasks[0].working_dir,
            Some("./packages/frontend".to_string())
        );
        assert_eq!(
            config.groups[0].tasks[1].working_dir,
            Some("./packages/backend".to_string())
        );

        // Daemon working_dir override
        assert_eq!(
            config.groups[0].daemons[0].working_dir,
            Some("./packages/server".to_string())
        );
    }

    #[test]
    fn test_working_dir_default() {
        let toml = r#"
[[group]]
name = "test"
patterns = ["*.txt"]

[[group.task]]
name = "default"
command = "echo hello"

[[group.daemon]]
name = "srv"
command = "./srv"
"#;
        let config = parse_toml(toml).unwrap();
        // Default should be None
        assert!(config.groups[0].working_dir.is_none());
        assert!(config.groups[0].tasks[0].working_dir.is_none());
        assert!(config.groups[0].daemons[0].working_dir.is_none());
    }

    #[test]
    fn test_daemon_delay_ms() {
        let toml = r#"
[[group]]
name = "test"
patterns = ["*.txt"]

[[group.daemon]]
name = "server"
command = "./server"
delay_ms = 500

[[group.daemon]]
name = "worker"
command = "./worker"
"#;
        let config = parse_toml(toml).unwrap();

        // Daemon with delay
        assert_eq!(config.groups[0].daemons[0].delay_ms, Some(500));
        // Daemon without delay (default)
        assert_eq!(config.groups[0].daemons[1].delay_ms, None);
    }

    #[test]
    fn test_env_parsing() {
        let toml = r#"
[[group]]
name = "test"
patterns = ["*.go"]
env = { GO_ENV = "test" }

[[group.task]]
name = "build"
command = "go build"
env = { CGO_ENABLED = "0", GOOS = "linux" }

[[group.daemon]]
name = "server"
command = "./server"
env = { PORT = "8080" }
"#;
        let config = parse_toml(toml).unwrap();

        // Group env
        assert_eq!(
            config.groups[0].env.get("GO_ENV"),
            Some(&"test".to_string())
        );

        // Task env
        assert_eq!(
            config.groups[0].tasks[0].env.get("CGO_ENABLED"),
            Some(&"0".to_string())
        );
        assert_eq!(
            config.groups[0].tasks[0].env.get("GOOS"),
            Some(&"linux".to_string())
        );

        // Daemon env
        assert_eq!(
            config.groups[0].daemons[0].env.get("PORT"),
            Some(&"8080".to_string())
        );
    }

    #[test]
    fn test_env_default() {
        let toml = r#"
[[group]]
name = "test"
patterns = ["*.txt"]

[[group.task]]
name = "default"
command = "echo hello"

[[group.daemon]]
name = "srv"
command = "./srv"
"#;
        let config = parse_toml(toml).unwrap();
        // Default should be empty
        assert!(config.groups[0].env.is_empty());
        assert!(config.groups[0].tasks[0].env.is_empty());
        assert!(config.groups[0].daemons[0].env.is_empty());
    }
}
