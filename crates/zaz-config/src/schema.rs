//! Configuration schema types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root configuration structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Global settings.
    pub settings: Settings,

    /// User-defined variables.
    pub variables: HashMap<String, String>,

    /// Watch groups.
    #[serde(alias = "group")]
    pub groups: Vec<Group>,
}

/// Global settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Shell to use for command execution (defaults to $SHELL).
    pub shell: Option<String>,

    /// Debounce time in milliseconds.
    pub debounce_ms: u64,

    /// Log output format.
    pub log_format: LogFormat,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shell: None,
            debounce_ms: 100,
            log_format: LogFormat::Pretty,
        }
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Plain,
    Json,
}

/// Log suppression level for tasks and daemons.
///
/// Controls which output streams are suppressed in the TUI.
/// Suppressed output is still captured for API/debugging purposes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Silence {
    /// No suppression - show all output (default).
    #[default]
    None,
    /// Suppress stdout only.
    Stdout,
    /// Suppress stderr only.
    Stderr,
    /// Suppress all output.
    All,
}

/// A watch group that pairs file patterns with commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Group {
    /// Unique name for this group.
    pub name: String,

    /// Glob patterns to watch.
    pub patterns: Vec<String>,

    /// Glob patterns to ignore.
    pub ignore: Vec<String>,

    /// Groups that must complete before this one runs.
    pub depends_on: Vec<String>,

    /// Working directory for commands (defaults to config file directory).
    pub working_dir: Option<String>,

    /// Environment variables for all commands in this group.
    /// These are merged with task/daemon-specific env vars.
    pub env: HashMap<String, String>,

    /// Task commands (run to completion).
    #[serde(alias = "task")]
    pub tasks: Vec<TaskCommand>,

    /// Daemon commands (long-running).
    #[serde(alias = "daemon")]
    pub daemons: Vec<DaemonCommand>,
}

/// A task command that runs to completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCommand {
    /// Display name for this command (derived from command if not set).
    #[serde(default)]
    name: Option<String>,

    /// Shell command to execute.
    pub command: String,

    /// Only run on file changes, not on initial startup.
    #[serde(default)]
    pub on_change_only: bool,

    /// Log suppression level.
    #[serde(default)]
    pub silence: Silence,

    /// Working directory for this command (overrides group working_dir).
    #[serde(default)]
    pub working_dir: Option<String>,

    /// Environment variables for this task (merged with group env).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl TaskCommand {
    /// Create a new task command with explicit name.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            command: command.into(),
            on_change_only: false,
            silence: Silence::None,
            working_dir: None,
            env: HashMap::new(),
        }
    }

    /// Create a task command where name is derived from command.
    pub fn from_command(command: impl Into<String>) -> Self {
        Self {
            name: None,
            command: command.into(),
            on_change_only: false,
            silence: Silence::None,
            working_dir: None,
            env: HashMap::new(),
        }
    }

    /// Get the display name, deriving from command if not explicitly set.
    ///
    /// For commands like "cargo build", derives "cargo build".
    /// For commands with flags, takes words until the first flag.
    pub fn name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| derive_name(&self.command))
    }

    /// Returns true if this task has an explicitly set name.
    pub fn has_explicit_name(&self) -> bool {
        self.name.is_some()
    }
}

/// Derive a display name from a command string.
///
/// Takes words until hitting a flag (starts with '-') or special char.
fn derive_name(command: &str) -> &str {
    let mut end = 0;
    for (i, c) in command.char_indices() {
        if c == '-' || c == '$' || c == '|' || c == '>' || c == '<' {
            break;
        }
        if !c.is_whitespace() {
            end = i + c.len_utf8();
        }
    }
    command[..end].trim()
}

/// A daemon command that runs continuously.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonCommand {
    /// Display name for this daemon (derived from command if not set).
    #[serde(default)]
    name: Option<String>,

    /// Shell command to execute.
    pub command: String,

    /// Signal to send when restarting.
    #[serde(default)]
    pub signal: Signal,

    /// Disable PTY allocation for this process.
    /// By default, PTY is enabled (no_pty = false).
    #[serde(default)]
    pub no_pty: bool,

    /// Log suppression level.
    #[serde(default)]
    pub silence: Silence,

    /// Working directory for this daemon (overrides group working_dir).
    #[serde(default)]
    pub working_dir: Option<String>,

    /// Delay in milliseconds before starting the daemon (after tasks complete).
    /// This is different from `debounce_ms` which controls file change batching.
    #[serde(default)]
    pub delay_ms: Option<u64>,

    /// Environment variables for this daemon (merged with group env).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl DaemonCommand {
    /// Create a new daemon command with explicit name.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            command: command.into(),
            signal: Signal::default(),
            no_pty: false,
            silence: Silence::None,
            working_dir: None,
            delay_ms: None,
            env: HashMap::new(),
        }
    }

    /// Create a daemon command where name is derived from command.
    pub fn from_command(command: impl Into<String>) -> Self {
        Self {
            name: None,
            command: command.into(),
            signal: Signal::default(),
            no_pty: false,
            silence: Silence::None,
            working_dir: None,
            delay_ms: None,
            env: HashMap::new(),
        }
    }

    /// Get the display name, deriving from command if not explicitly set.
    pub fn name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| derive_name(&self.command))
    }

    /// Returns true if this daemon has an explicitly set name.
    pub fn has_explicit_name(&self) -> bool {
        self.name.is_some()
    }
}

/// Unix signals for daemon control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Signal {
    #[default]
    Sigterm,
    Sigint,
    Sighup,
    Sigkill,
    Sigquit,
    Sigusr1,
    Sigusr2,
}
