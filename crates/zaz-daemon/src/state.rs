//! Daemon state management.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Overall daemon state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonState {
    /// Current status.
    pub status: DaemonStatus,

    /// Watch groups (ordered by config file order).
    pub groups: IndexMap<String, GroupState>,

    /// Number of files being watched.
    pub watched_files: usize,

    /// Last file change timestamp (Unix millis).
    pub last_change: Option<u64>,

    /// Why the first load failed, when it did. Set only while nothing is built. A later reload
    /// failure never sets it, and a successful load clears it.
    ///
    /// This is a field rather than a status variant so a client that predates it still
    /// deserializes the state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<LoadError>,

    /// Errors per workspace member, reported only by a workspace supervisor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_errors: Vec<MemberError>,
}

/// A coded failure to load a config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadError {
    /// Stable machine-readable code of the first error.
    pub code: String,

    /// Every error, one per line, each ending in its code.
    pub message: String,

    /// What to do next, when the first error carries a hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl LoadError {
    /// Build an error from its parts.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: None,
        }
    }

    /// Attach a hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// The error of one workspace member, named by its project token or by its config path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberError {
    /// The member's project token, or its config path when it has none.
    pub member: String,

    /// What went wrong.
    pub error: LoadError,
}

/// Daemon status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonStatus {
    /// Starting up.
    #[default]
    Starting,

    /// Running normally.
    Running,

    /// Shutting down.
    Stopping,
}

/// State of a watch group.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupState {
    /// Group name.
    pub name: String,

    /// Current status.
    pub status: GroupStatus,

    /// Task command states.
    pub tasks: Vec<ProcessState>,

    /// Service states.
    ///
    /// The `daemons` alias reads state emitted by a pre-rename daemon still
    /// running across an upgrade.
    #[serde(alias = "daemons")]
    pub services: Vec<ProcessState>,
}

/// Group status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupStatus {
    /// Not yet run.
    #[default]
    Pending,

    /// Waiting for dependencies to complete.
    Waiting,

    /// Tasks are running.
    Running,

    /// All tasks completed, services running.
    Ready,

    /// A task failed.
    Failed,

    /// Skipped due to dependency failure.
    Skipped,
}

/// State of a single process (task or service).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessState {
    /// Process name.
    pub name: String,

    /// Current status.
    pub status: ProcessStatus,

    /// Process ID (if running).
    pub pid: Option<u32>,

    /// Exit code (if exited).
    pub exit_code: Option<i32>,

    /// Duration of last run in milliseconds.
    pub duration_ms: Option<u64>,
}

/// Process status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessStatus {
    /// Not yet started.
    #[default]
    Pending,

    /// Spawned, but its `ready_check` has not passed yet. Only a service configuring a
    /// readiness check ever reaches this state.
    Starting,

    /// Currently running.
    Running,

    /// Completed successfully.
    Success,

    /// Failed.
    Failed,

    /// Waiting to restart (backoff).
    Backoff,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_without_load_fields_deserializes_from_an_older_daemon() {
        let older = r#"{"status":"running","groups":{},"watched_files":0,"last_change":null}"#;

        let state: DaemonState = serde_json::from_str(older).unwrap();

        assert_eq!(state.status, DaemonStatus::Running);
        assert!(state.load_error.is_none());
        assert!(state.member_errors.is_empty());
    }

    #[test]
    fn a_state_without_errors_serializes_without_the_new_fields() {
        let json = serde_json::to_string(&DaemonState::default()).unwrap();

        assert!(!json.contains("load_error"), "{json}");
        assert!(!json.contains("member_errors"), "{json}");
    }

    #[test]
    fn load_error_round_trips_with_its_hint() {
        let state = DaemonState {
            load_error: Some(LoadError::new("env_file_missing", "no file").with_hint("create it")),
            ..Default::default()
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: DaemonState = serde_json::from_str(&json).unwrap();

        assert_eq!(back.load_error, state.load_error);
    }

    #[test]
    fn group_state_serializes_services_key() {
        let group = GroupState {
            name: "server".to_string(),
            services: vec![ProcessState {
                name: "web".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let json = serde_json::to_string(&group).unwrap();
        assert!(
            json.contains("\"services\""),
            "expected services key in {json}"
        );
        assert!(
            !json.contains("\"daemons\""),
            "daemons key must not be emitted: {json}"
        );
    }

    #[test]
    fn group_state_reads_legacy_daemons_key() {
        let legacy = r#"{"name":"server","status":"ready","tasks":[],"daemons":[{"name":"web","status":"running","pid":4242,"exit_code":null,"duration_ms":null}]}"#;

        let group: GroupState = serde_json::from_str(legacy).unwrap();
        assert_eq!(group.services.len(), 1);
        assert_eq!(group.services[0].name, "web");
        assert_eq!(group.services[0].pid, Some(4242));
    }
}
