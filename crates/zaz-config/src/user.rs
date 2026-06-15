//! User-level configuration for zaz.
//!
//! User config is stored separately from project config to allow per-user preferences
//! that shouldn't be committed to version control.
//!
//! Default location: `~/.config/zaz/config.toml` (following XDG Base Directory spec)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// User-level configuration preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UserConfig {
    /// Disable auto-starting a daemon before the TUI opens.
    pub no_autostart: bool,

    /// Disable blinking/animation effects in the TUI.
    pub disable_animations: bool,

    /// Default TUI style preference.
    pub tui_style: Option<TuiStylePreference>,

    /// Log colorization settings.
    pub log_colors: LogColorConfig,

    /// Notification settings.
    pub notifications: NotificationConfig,

    /// Log storage settings.
    pub log_storage: LogStorageConfig,
}

/// Log storage backend selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LogStorageBackend {
    /// In-memory ring buffer only. Logs are lost on daemon exit.
    #[default]
    #[serde(rename = "memory")]
    Memory,
    /// Persistent SQLite-backed storage plus an in-memory hot buffer.
    #[serde(rename = "sqlite")]
    Sqlite,
}

/// Log storage configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogStorageConfig {
    /// Storage backend selector. Default: `memory`.
    pub backend: LogStorageBackend,

    /// Maximum memory budget for the in-memory hot buffer.
    /// When approached, oldest logs are evicted.
    /// Default: 100MB. Use human-readable formats: "100MB", "1GB", etc.
    /// The legacy field name `memory_limit` is accepted as an alias.
    #[serde(default = "default_hot_memory_limit", alias = "memory_limit")]
    pub hot_memory_limit: String,

    /// Maximum log lines to keep per process in the hot buffer.
    /// Default: 100000. The legacy field name `max_lines_per_process` is
    /// accepted as an alias.
    #[serde(
        default = "default_hot_max_lines_per_process",
        alias = "max_lines_per_process"
    )]
    pub hot_max_lines_per_process: usize,

    /// SQLite-backend retention settings. Honored when `backend = "sqlite"`.
    pub sqlite: SqliteStorageConfig,
}

fn default_hot_memory_limit() -> String {
    "100MB".to_string()
}

fn default_hot_max_lines_per_process() -> usize {
    100_000
}

impl Default for LogStorageConfig {
    fn default() -> Self {
        Self {
            backend: LogStorageBackend::default(),
            hot_memory_limit: default_hot_memory_limit(),
            hot_max_lines_per_process: default_hot_max_lines_per_process(),
            sqlite: SqliteStorageConfig::default(),
        }
    }
}

impl LogStorageConfig {
    /// Parse the hot-buffer memory limit string and return bytes.
    /// Supports formats like "100MB", "1GB", "500KB", or plain number (bytes).
    pub fn hot_memory_limit_bytes(&self) -> usize {
        parse_byte_size(&self.hot_memory_limit).unwrap_or(100 * 1024 * 1024)
    }
}

/// SQLite-backed log storage retention configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SqliteStorageConfig {
    /// Maximum on-disk database size. Oldest rows are pruned when approached.
    /// Default: 512MB. Use human-readable formats: "512MB", "1GB", etc.
    #[serde(default = "default_sqlite_max_size")]
    pub max_size: String,

    /// Maximum persisted log lines to keep per process.
    /// Default: 250000.
    #[serde(default = "default_sqlite_max_lines_per_process")]
    pub max_lines_per_process: usize,
}

fn default_sqlite_max_size() -> String {
    "512MB".to_string()
}

fn default_sqlite_max_lines_per_process() -> usize {
    250_000
}

impl Default for SqliteStorageConfig {
    fn default() -> Self {
        Self {
            max_size: default_sqlite_max_size(),
            max_lines_per_process: default_sqlite_max_lines_per_process(),
        }
    }
}

impl SqliteStorageConfig {
    /// Parse the persistent max-size string and return bytes.
    pub fn max_size_bytes(&self) -> usize {
        parse_byte_size(&self.max_size).unwrap_or(512 * 1024 * 1024)
    }
}

/// Parse a human-readable byte size string.
/// Supports: B, KB, MB, GB (case-insensitive).
/// Returns None if parsing fails.
fn parse_byte_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Try plain number first
    if let Ok(n) = s.parse::<usize>() {
        return Some(n);
    }

    // Find where the number ends and suffix begins
    let s_upper = s.to_uppercase();
    let (num_part, suffix) = if s_upper.ends_with("GB") {
        (&s[..s.len() - 2], "GB")
    } else if s_upper.ends_with("MB") {
        (&s[..s.len() - 2], "MB")
    } else if s_upper.ends_with("KB") {
        (&s[..s.len() - 2], "KB")
    } else if s_upper.ends_with('B') {
        (&s[..s.len() - 1], "B")
    } else {
        return None;
    };

    let num: f64 = num_part.trim().parse().ok()?;
    let multiplier = match suffix {
        "GB" => 1024 * 1024 * 1024,
        "MB" => 1024 * 1024,
        "KB" => 1024,
        "B" => 1,
        _ => return None,
    };

    Some((num * multiplier as f64) as usize)
}

/// Log colorization configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogColorConfig {
    /// Preserve ANSI colors from command output (default: true).
    pub preserve_ansi: bool,

    /// Pattern-based colorization rules.
    /// Each rule maps a regex pattern to a color name.
    pub rules: Vec<ColorRule>,

    /// Enable JSON log parsing (default: false).
    /// When enabled, attempts to parse JSON logs and extract structured fields.
    pub parse_json: bool,

    /// JSON field to use as log level (e.g., "level", "severity").
    pub json_level_field: Option<String>,

    /// JSON field to use as message (e.g., "msg", "message").
    pub json_message_field: Option<String>,
}

impl Default for LogColorConfig {
    fn default() -> Self {
        Self {
            preserve_ansi: true,
            rules: vec![
                ColorRule {
                    pattern: "(?i)\\berror\\b".to_string(),
                    color: "red".to_string(),
                },
                ColorRule {
                    pattern: "(?i)\\bwarn(ing)?\\b".to_string(),
                    color: "yellow".to_string(),
                },
                ColorRule {
                    pattern: "(?i)\\binfo\\b".to_string(),
                    color: "green".to_string(),
                },
                ColorRule {
                    pattern: "(?i)\\bdebug\\b".to_string(),
                    color: "gray".to_string(),
                },
            ],
            parse_json: false,
            json_level_field: Some("level".to_string()),
            json_message_field: Some("msg".to_string()),
        }
    }
}

/// A pattern-based colorization rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorRule {
    /// Regex pattern to match against log content.
    pub pattern: String,

    /// Color name: red, green, yellow, blue, magenta, cyan, white, gray.
    pub color: String,
}

/// Notification configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    /// Enable desktop notifications (default: false).
    pub enabled: bool,

    /// Show notification on task failure (default: true when enabled).
    pub on_failure: bool,

    /// Show notification on task success (default: false).
    pub on_success: bool,

    /// Show notification when all groups complete (default: true when enabled).
    pub on_group_complete: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on_failure: true,
            on_success: false,
            on_group_complete: true,
        }
    }
}

/// TUI style preference for user config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TuiStylePreference {
    /// Full style with group tree and logs pane.
    #[default]
    #[serde(rename = "full")]
    Full,
    /// Multi-pane style with one pane per task.
    #[serde(rename = "multi_pane", alias = "minimal")]
    MultiPane,
}

/// Get the path to the user configuration file.
///
/// Uses XDG Base Directory specification:
/// - `$XDG_CONFIG_HOME/zaz/config.toml` if set
/// - Otherwise `~/.config/zaz/config.toml`
pub fn user_config_path() -> PathBuf {
    let config_dir = if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg_config)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        // Fallback to current directory (unlikely to be used)
        PathBuf::from(".")
    };

    config_dir.join("zaz").join("config.toml")
}

/// Load user configuration from the default location.
///
/// Returns default config if the file doesn't exist or can't be parsed.
/// This is intentional - user config is optional and shouldn't block startup.
pub fn load_user_config() -> UserConfig {
    let path = user_config_path();
    load_user_config_from(&path)
}

/// Load user configuration from a specific path.
///
/// Returns default config if the file doesn't exist or can't be parsed.
pub fn load_user_config_from(path: &PathBuf) -> UserConfig {
    match std::fs::read_to_string(path) {
        Ok(contents) => match toml::from_str(&contents) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to parse user config, using defaults"
                );
                UserConfig::default()
            }
        },
        Err(_) => {
            // File doesn't exist - this is normal, not an error
            UserConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_user_config() {
        let config = UserConfig::default();
        assert!(!config.no_autostart);
        assert!(!config.disable_animations);
        assert!(config.tui_style.is_none());
    }

    #[test]
    fn test_parse_user_config() {
        let toml = r#"
no_autostart = true
disable_animations = true
tui_style = "multi_pane"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert!(config.no_autostart);
        assert!(config.disable_animations);
        assert_eq!(config.tui_style, Some(TuiStylePreference::MultiPane));
    }

    #[test]
    fn test_parse_legacy_minimal_alias() {
        // "minimal" should still work as an alias for "multi_pane"
        let toml = r#"
tui_style = "minimal"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tui_style, Some(TuiStylePreference::MultiPane));
    }

    #[test]
    fn test_parse_partial_config() {
        // Only some fields set - others should use defaults
        let toml = r#"
tui_style = "full"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert!(!config.no_autostart);
        assert!(!config.disable_animations);
        assert_eq!(config.tui_style, Some(TuiStylePreference::Full));
    }

    #[test]
    fn test_parse_empty_config() {
        let config: UserConfig = toml::from_str("").unwrap();
        assert!(!config.no_autostart);
        assert!(!config.disable_animations);
        assert!(config.tui_style.is_none());
    }

    #[test]
    fn test_user_config_path() {
        let path = user_config_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("zaz") && path_str.contains("config.toml"));
    }

    #[test]
    fn test_load_nonexistent_config() {
        let path = PathBuf::from("/nonexistent/path/config.toml");
        let config = load_user_config_from(&path);
        // Should return defaults, not error
        assert!(!config.no_autostart);
    }

    #[test]
    fn test_parse_byte_size() {
        // Plain numbers
        assert_eq!(parse_byte_size("1024"), Some(1024));
        assert_eq!(parse_byte_size("0"), Some(0));

        // Bytes
        assert_eq!(parse_byte_size("100B"), Some(100));
        assert_eq!(parse_byte_size("100b"), Some(100));

        // Kilobytes
        assert_eq!(parse_byte_size("1KB"), Some(1024));
        assert_eq!(parse_byte_size("1kb"), Some(1024));
        assert_eq!(parse_byte_size("10KB"), Some(10 * 1024));

        // Megabytes
        assert_eq!(parse_byte_size("1MB"), Some(1024 * 1024));
        assert_eq!(parse_byte_size("100MB"), Some(100 * 1024 * 1024));
        assert_eq!(parse_byte_size("100mb"), Some(100 * 1024 * 1024));

        // Gigabytes
        assert_eq!(parse_byte_size("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_byte_size("2GB"), Some(2 * 1024 * 1024 * 1024));

        // Fractional values
        assert_eq!(
            parse_byte_size("1.5MB"),
            Some((1.5 * 1024.0 * 1024.0) as usize)
        );
        assert_eq!(
            parse_byte_size("0.5GB"),
            Some((0.5 * 1024.0 * 1024.0 * 1024.0) as usize)
        );

        // With whitespace
        assert_eq!(parse_byte_size("  100MB  "), Some(100 * 1024 * 1024));
        assert_eq!(parse_byte_size("100 MB"), Some(100 * 1024 * 1024));

        // Invalid
        assert_eq!(parse_byte_size(""), None);
        assert_eq!(parse_byte_size("invalid"), None);
        assert_eq!(parse_byte_size("100TB"), None); // TB not supported
    }

    #[test]
    fn test_log_storage_config_default() {
        let config = LogStorageConfig::default();
        assert_eq!(config.backend, LogStorageBackend::Memory);
        assert_eq!(config.hot_memory_limit, "100MB");
        assert_eq!(config.hot_max_lines_per_process, 100_000);
        assert_eq!(config.hot_memory_limit_bytes(), 100 * 1024 * 1024);
        assert_eq!(config.sqlite.max_size, "512MB");
        assert_eq!(config.sqlite.max_lines_per_process, 250_000);
    }

    #[test]
    fn test_parse_log_storage_config() {
        let toml = r#"
[log_storage]
hot_memory_limit = "200MB"
hot_max_lines_per_process = 50000
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.backend, LogStorageBackend::Memory);
        assert_eq!(config.log_storage.hot_memory_limit, "200MB");
        assert_eq!(config.log_storage.hot_max_lines_per_process, 50000);
        assert_eq!(
            config.log_storage.hot_memory_limit_bytes(),
            200 * 1024 * 1024
        );
    }

    #[test]
    fn test_log_storage_config_partial() {
        // Only hot_memory_limit set
        let toml = r#"
[log_storage]
hot_memory_limit = "1GB"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.hot_memory_limit, "1GB");
        assert_eq!(config.log_storage.hot_max_lines_per_process, 100_000); // default
        assert_eq!(
            config.log_storage.hot_memory_limit_bytes(),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn test_log_storage_backend_default() {
        let config: UserConfig = toml::from_str("").unwrap();
        assert_eq!(config.log_storage.backend, LogStorageBackend::Memory);
    }

    #[test]
    fn test_parse_log_storage_memory_backend_explicit() {
        let toml = r#"
[log_storage]
backend = "memory"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.backend, LogStorageBackend::Memory);
        // Other fields retain defaults.
        assert_eq!(config.log_storage.hot_memory_limit, "100MB");
        assert_eq!(config.log_storage.sqlite.max_size, "512MB");
    }

    #[test]
    fn test_parse_log_storage_sqlite_backend() {
        let toml = r#"
[log_storage]
backend = "sqlite"
hot_memory_limit = "16MB"
hot_max_lines_per_process = 10000

[log_storage.sqlite]
max_size = "1GB"
max_lines_per_process = 500000
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.backend, LogStorageBackend::Sqlite);
        assert_eq!(config.log_storage.hot_memory_limit, "16MB");
        assert_eq!(config.log_storage.hot_max_lines_per_process, 10_000);
        assert_eq!(
            config.log_storage.hot_memory_limit_bytes(),
            16 * 1024 * 1024
        );
        assert_eq!(config.log_storage.sqlite.max_size, "1GB");
        assert_eq!(config.log_storage.sqlite.max_lines_per_process, 500_000);
        assert_eq!(
            config.log_storage.sqlite.max_size_bytes(),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn test_legacy_memory_limit_alias() {
        // Old field name still parses into hot_memory_limit.
        let toml = r#"
[log_storage]
memory_limit = "200MB"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.hot_memory_limit, "200MB");
        assert_eq!(
            config.log_storage.hot_memory_limit_bytes(),
            200 * 1024 * 1024
        );
        assert_eq!(config.log_storage.hot_max_lines_per_process, 100_000);
    }

    #[test]
    fn test_legacy_max_lines_per_process_alias() {
        let toml = r#"
[log_storage]
max_lines_per_process = 50000
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.hot_max_lines_per_process, 50_000);
        assert_eq!(config.log_storage.hot_memory_limit, "100MB");
    }

    #[test]
    fn test_sqlite_storage_config_default() {
        let config = SqliteStorageConfig::default();
        assert_eq!(config.max_size, "512MB");
        assert_eq!(config.max_lines_per_process, 250_000);
        assert_eq!(config.max_size_bytes(), 512 * 1024 * 1024);
    }

    #[test]
    fn test_parse_sqlite_section_partial() {
        // Only sqlite.max_size set; sibling field falls back to default.
        let toml = r#"
[log_storage.sqlite]
max_size = "2GB"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.sqlite.max_size, "2GB");
        assert_eq!(config.log_storage.sqlite.max_lines_per_process, 250_000);
        assert_eq!(
            config.log_storage.sqlite.max_size_bytes(),
            2 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn test_log_storage_unknown_field_tolerated() {
        // User config is permissive (see docs/user-configuration.md);
        // unknown keys should not break parsing.
        let toml = r#"
[log_storage]
backend = "memory"
unknown_future_option = "ignored"
"#;
        let config: UserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.log_storage.backend, LogStorageBackend::Memory);
        assert_eq!(config.log_storage.hot_memory_limit, "100MB");
    }
}
