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
    /// Disable auto-starting daemon when TUI starts.
    pub no_autostart: bool,

    /// Disable blinking/animation effects in the TUI.
    pub disable_animations: bool,

    /// Default TUI style preference.
    pub tui_style: Option<TuiStylePreference>,

    /// Log colorization settings.
    pub log_colors: LogColorConfig,

    /// Notification settings.
    pub notifications: NotificationConfig,
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
}
