//! The loaded-environment report.
//!
//! The report names the keys the running config loaded and where each came from. It never holds a
//! value, since values often carry secrets and the report reaches model context through the MCP
//! tool. Everything in it is a key name, a config field, or a source as the config wrote it. The
//! one exception is the text of a load or refresh error, which carries a failed command's capped,
//! stripped stderr.

use crate::state::{DaemonStatus, LoadError};
use serde::{Deserialize, Serialize};

/// The loaded environment of the running config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvReport {
    /// The workspace project this report describes, set only by a supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,

    /// The daemon's status. A report with no groups and `Starting` means nothing is loaded yet.
    pub status: DaemonStatus,

    /// Why the first load failed, when it did. No keys are reported then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<LoadError>,

    /// Why the last refresh of a watched source failed, when it did.
    ///
    /// The groups below still show the environment the daemon is running on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<LoadError>,

    /// The inherited variable names the base environment dropped, sorted.
    ///
    /// Empty when no level sets an `env_command`, since the base is then never used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_inherited: Vec<String>,

    /// Every group, in config order.
    #[serde(default)]
    pub groups: Vec<GroupEnvReport>,
}

/// The environment sources of one group and of each of its processes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupEnvReport {
    pub name: String,

    /// What the group's own environment starts from.
    pub starts_from: EnvStart,

    /// The sources that apply to the group, in the order they are applied.
    pub sources: Vec<EnvSourceReport>,

    pub tasks: Vec<ProcessEnvReport>,
    pub services: Vec<ProcessEnvReport>,
}

/// The environment sources of one task or service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEnvReport {
    pub name: String,

    /// What the process's environment starts from.
    pub starts_from: EnvStart,

    /// The sources that apply to the process, in the order they are applied.
    ///
    /// They run from the settings level through the group's and then the process's own.
    pub sources: Vec<EnvSourceReport>,
}

/// The environment a chain of sources is applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvStart {
    /// The daemon's own environment.
    Daemon,
    /// The fixed base environment, plus any `env_passthrough` names.
    Base,
}

/// The config level a source was declared at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvLevel {
    Settings,
    Group,
    Task,
    Service,
}

/// The kind of source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvSourceKind {
    /// An `env_file`.
    File,
    /// An `env_command`.
    Command,
    /// An inline `env` table.
    Inline,
}

/// The key names one source set and unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvSourceReport {
    pub level: EnvLevel,

    /// The config field, such as `group 'api' env_file`.
    pub field: String,

    pub kind: EnvSourceKind,

    /// The `env_file` path or the `env_command` as written in the config. Unset for an inline
    /// table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,

    /// The names the source set, sorted.
    pub set: Vec<String>,

    /// The names the source removed, sorted. Only an `env_command` can unset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset: Vec<String>,

    /// Whether the source set and unset nothing.
    ///
    /// An `env_command` that prints nothing, such as `direnv export json` with a missing or
    /// unallowed `.envrc`, looks like this.
    pub produced_no_keys: bool,
}
