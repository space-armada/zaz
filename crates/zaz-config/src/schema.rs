//! Configuration schema types.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::time::Duration;

/// A duration that can be specified as either:
/// - A human-readable string: "500ms", "2s", "1m30s"
/// - An integer (interpreted as milliseconds for backwards compatibility)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(Duration);

impl HumanDuration {
    /// Create a new HumanDuration from a Duration.
    pub fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// Create a new HumanDuration from milliseconds.
    pub fn from_millis(ms: u64) -> Self {
        Self(Duration::from_millis(ms))
    }

    /// Get the underlying Duration.
    pub fn as_duration(&self) -> Duration {
        self.0
    }

    /// Get the duration in milliseconds.
    pub fn as_millis(&self) -> u64 {
        self.0.as_millis() as u64
    }
}

impl Default for HumanDuration {
    fn default() -> Self {
        Self(Duration::from_millis(100))
    }
}

impl From<Duration> for HumanDuration {
    fn from(d: Duration) -> Self {
        Self(d)
    }
}

impl From<HumanDuration> for Duration {
    fn from(hd: HumanDuration) -> Self {
        hd.0
    }
}

impl Serialize for HumanDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize as a human-readable string
        let formatted = humantime::format_duration(self.0).to_string();
        serializer.serialize_str(&formatted)
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct HumanDurationVisitor;

        impl<'de> Visitor<'de> for HumanDurationVisitor {
            type Value = HumanDuration;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter
                    .write_str("a duration string (e.g., '500ms', '2s') or integer milliseconds")
            }

            fn visit_str<E>(self, value: &str) -> Result<HumanDuration, E>
            where
                E: de::Error,
            {
                humantime::parse_duration(value)
                    .map(HumanDuration)
                    .map_err(|e| de::Error::custom(format!("invalid duration '{}': {}", value, e)))
            }

            fn visit_u64<E>(self, value: u64) -> Result<HumanDuration, E>
            where
                E: de::Error,
            {
                Ok(HumanDuration(Duration::from_millis(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<HumanDuration, E>
            where
                E: de::Error,
            {
                if value < 0 {
                    Err(de::Error::custom("duration cannot be negative"))
                } else {
                    Ok(HumanDuration(Duration::from_millis(value as u64)))
                }
            }
        }

        deserializer.deserialize_any(HumanDurationVisitor)
    }
}

/// Root configuration structure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Shell to use for command execution (defaults to $SHELL).
    pub shell: Option<String>,

    /// Debounce time for file change batching.
    /// Accepts human-readable strings ("100ms", "1s") or integer milliseconds.
    #[serde(alias = "debounce_ms")]
    pub debounce: HumanDuration,

    /// Log output format.
    pub log_format: LogFormat,

    /// Optional project token for workspace addressing. When unset, the
    /// supervisor derives the token from the config directory basename.
    #[serde(default)]
    pub name: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shell: None,
            debounce: HumanDuration::from_millis(100),
            log_format: LogFormat::Pretty,
            name: None,
        }
    }
}

impl Settings {
    /// Get debounce time in milliseconds (for backwards compatibility).
    pub fn debounce_ms(&self) -> u64 {
        self.debounce.as_millis()
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

/// Log suppression level for tasks and services.
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Service commands (long-running).
    #[serde(alias = "service", alias = "daemon", alias = "daemons")]
    pub services: Vec<ServiceCommand>,
}

/// A task command that runs to completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// A service command that runs continuously.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceCommand {
    /// Display name for this service (derived from command if not set).
    #[serde(default)]
    name: Option<String>,

    /// Shell command to execute.
    pub command: String,

    /// Command run immediately before every spawn of this service, including the first
    /// start of a freshly-started group.
    ///
    /// Clears state a prior run left behind, such as a stale container, lockfile, or PID
    /// file. A failure is logged and the spawn proceeds.
    #[serde(default)]
    pub cleanup_command: Option<String>,

    /// Command that reports whether this service has finished starting. A zero exit means
    /// ready; any nonzero exit means not yet.
    ///
    /// Unset means a service counts as ready the moment it spawns, which is how every service
    /// behaved before this field existed.
    #[serde(default)]
    pub ready_check: Option<String>,

    /// Command that stops this service, replacing the signal sent to its process group.
    ///
    /// Lets a service that wraps an external system stop what it wraps rather than only its
    /// local process. A failure is logged and the stop proceeds.
    #[serde(default)]
    pub stop_command: Option<String>,

    /// Command that force kills this service, replacing the SIGKILL sent to its process
    /// group once `stop_timeout` elapses.
    #[serde(default)]
    pub kill_command: Option<String>,

    /// Signal to send when restarting.
    ///
    /// Unset falls back to SIGTERM. The distinction between unset and an explicit SIGTERM
    /// matters: setting a signal alongside `stop_command`, which replaces the signal
    /// outright, is a validation error.
    #[serde(default)]
    signal: Option<Signal>,

    /// Disable PTY allocation for this process.
    /// By default, PTY is enabled (no_pty = false).
    #[serde(default)]
    pub no_pty: bool,

    /// Log suppression level.
    #[serde(default)]
    pub silence: Silence,

    /// Working directory for this service (overrides group working_dir).
    #[serde(default)]
    pub working_dir: Option<String>,

    /// Delay before starting the service (after tasks complete).
    /// Accepts human-readable strings ("500ms", "2s") or integer milliseconds.
    /// This is different from `debounce` which controls file change batching.
    #[serde(default, alias = "delay_ms")]
    pub delay: Option<HumanDuration>,

    /// How long to wait for the service to exit after its stop signal before escalating to
    /// SIGKILL. Applies to every restart and to daemon shutdown.
    ///
    /// Accepts human-readable strings ("30s", "2m") or integer milliseconds. Unset falls
    /// back to a 10-second default.
    #[serde(default)]
    pub stop_timeout: Option<HumanDuration>,

    /// How long to wait between `ready_check` runs.
    ///
    /// Accepts human-readable strings ("250ms", "1s") or integer milliseconds. Unset falls
    /// back to a 100-millisecond default. Setting this without `ready_check` is a validation
    /// error.
    #[serde(default)]
    pub ready_poll_interval: Option<HumanDuration>,

    /// How long `ready_check` may keep failing before the service is treated as failed to
    /// start.
    ///
    /// Accepts human-readable strings ("30s", "2m") or integer milliseconds. Unset falls back
    /// to a 30-second default, which is longer than the stop timeout because legitimate
    /// startup times vary far more widely than shutdowns do. Setting this without
    /// `ready_check` is a validation error.
    #[serde(default)]
    pub ready_timeout: Option<HumanDuration>,

    /// Environment variables for this service (merged with group env).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl ServiceCommand {
    /// Create a new service command with explicit name.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            command: command.into(),
            cleanup_command: None,
            ready_check: None,
            stop_command: None,
            kill_command: None,
            signal: None,
            no_pty: false,
            silence: Silence::None,
            working_dir: None,
            delay: None,
            stop_timeout: None,
            ready_poll_interval: None,
            ready_timeout: None,
            env: HashMap::new(),
        }
    }

    /// Create a service command where name is derived from command.
    pub fn from_command(command: impl Into<String>) -> Self {
        Self {
            name: None,
            command: command.into(),
            cleanup_command: None,
            ready_check: None,
            stop_command: None,
            kill_command: None,
            signal: None,
            no_pty: false,
            silence: Silence::None,
            working_dir: None,
            delay: None,
            stop_timeout: None,
            ready_poll_interval: None,
            ready_timeout: None,
            env: HashMap::new(),
        }
    }

    /// Get the display name, deriving from command if not explicitly set.
    pub fn name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| derive_name(&self.command))
    }

    /// Returns true if this service has an explicitly set name.
    pub fn has_explicit_name(&self) -> bool {
        self.name.is_some()
    }

    /// Set the restart signal explicitly.
    pub fn with_signal(mut self, signal: Signal) -> Self {
        self.signal = Some(signal);
        self
    }

    /// Get the restart signal, falling back to SIGTERM when unset.
    pub fn signal(&self) -> Signal {
        self.signal.unwrap_or_default()
    }

    /// Returns true if this service has an explicitly set restart signal.
    pub fn has_explicit_signal(&self) -> bool {
        self.signal.is_some()
    }

    /// Get delay in milliseconds (for backwards compatibility).
    pub fn delay_ms(&self) -> Option<u64> {
        self.delay.map(|d| d.as_millis())
    }

    /// Get the configured stop timeout in milliseconds.
    ///
    /// Returns None when unset, leaving the default to the process layer.
    pub fn stop_timeout_ms(&self) -> Option<u64> {
        self.stop_timeout.map(|t| t.as_millis())
    }

    /// Get the configured readiness poll interval in milliseconds.
    ///
    /// Returns None when unset, leaving the default to the readiness poller.
    pub fn ready_poll_interval_ms(&self) -> Option<u64> {
        self.ready_poll_interval.map(|t| t.as_millis())
    }

    /// Get the configured readiness timeout in milliseconds.
    ///
    /// Returns None when unset, leaving the default to the readiness poller.
    pub fn ready_timeout_ms(&self) -> Option<u64> {
        self.ready_timeout.map(|t| t.as_millis())
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
