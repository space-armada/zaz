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
