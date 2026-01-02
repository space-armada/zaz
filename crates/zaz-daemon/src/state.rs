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

    /// Daemon states.
    pub daemons: Vec<ProcessState>,
}

/// Group status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupStatus {
    /// Not yet run.
    #[default]
    Pending,

    /// Tasks are running.
    Running,

    /// All tasks completed, daemons running.
    Ready,

    /// A task failed.
    Failed,
}

/// State of a single process (task or daemon).
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
