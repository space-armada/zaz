//! MCP-facing response types for zaz tools.
//!
//! These mirror the daemon and config types but are owned by the MCP crate so
//! the agent-facing schema stays stable across internal refactors and so
//! `schemars` derives stay contained here.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zaz_config::{Config, DaemonCommand, Group, LogFormat, Settings, Signal, Silence, TaskCommand};
use zaz_daemon::{
    DaemonState, DaemonStatus, GroupState, GroupStatus, LogLine, LogSource, OutputKind,
    ProcessState, ProcessStatus,
};

/// Top-level daemon status response for the `zaz_status` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatusReport {
    /// Overall lifecycle state of the daemon.
    pub daemon_status: DaemonStatusReport,
    /// Number of files currently being watched.
    pub watched_files: usize,
    /// Unix milliseconds of the most recent file-change event, if any.
    pub last_change_ms: Option<u64>,
    /// All configured groups in declaration order.
    pub groups: Vec<GroupReport>,
}

/// Daemon lifecycle state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DaemonStatusReport {
    Starting,
    Running,
    Stopping,
}

/// State of a single group, with tasks and daemons collapsed into a uniform
/// `processes` list distinguished by a `kind` field.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GroupReport {
    pub name: String,
    pub status: GroupStatusReport,
    pub processes: Vec<ProcessReport>,
}

/// Group status as exposed via MCP.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GroupStatusReport {
    Pending,
    Waiting,
    Running,
    Ready,
    Failed,
    Skipped,
}

/// State of a single managed process (task or daemon).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessReport {
    pub name: String,
    pub kind: ProcessKind,
    pub status: ProcessStatusReport,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
}

/// Whether a process is a task (run-to-completion) or a daemon (long-running).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProcessKind {
    Task,
    Daemon,
}

/// Process status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProcessStatusReport {
    Pending,
    Running,
    Success,
    Failed,
    Backoff,
}

/// Slim group listing for the `zaz_list_groups` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GroupsReport {
    pub groups: Vec<GroupSummary>,
}

/// One row in the slim group listing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GroupSummary {
    pub name: String,
    pub status: GroupStatusReport,
    pub task_count: usize,
    pub daemon_count: usize,
}

/// Input parameters for the `zaz_logs` tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LogsRequest {
    /// Process name to query. Use "*" or omit for all processes.
    #[serde(default)]
    pub name: Option<String>,
    /// Project token selecting one workspace member to query. Required against a
    /// workspace supervisor; omit for a single-config daemon. A query is always
    /// scoped to one member, never merged across the working set.
    #[serde(default)]
    pub project: Option<String>,
    /// Number of leading entries to skip for pagination.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of entries to return on this page.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Case-insensitive substring filter applied to log content.
    #[serde(default)]
    pub search: Option<String>,
}

/// Input parameters for the `zaz_restart_group` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RestartGroupRequest {
    /// Name of the group to restart. Must match a group declared in `zaz.toml`/`zaz.json`.
    pub name: String,
    /// Project token selecting one workspace member. Set against a workspace
    /// supervisor; omit for a single-config daemon.
    #[serde(default)]
    pub project: Option<String>,
}

/// Input parameters for the `zaz_restart_process` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RestartProcessRequest {
    /// Group the process belongs to.
    pub group: String,
    /// Process name. Matches the `name` field of a task or daemon entry inside the group.
    pub process: String,
    /// Project token selecting one workspace member. Set against a workspace
    /// supervisor; omit for a single-config daemon.
    #[serde(default)]
    pub project: Option<String>,
}

/// Confirmation response returned by all mutation tools.
///
/// Carries the daemon's free-form acknowledgement string so the agent can
/// surface it verbatim. Daemon-side failures are returned as MCP errors, not
/// as a `MutationReport` with an error flag.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MutationReport {
    /// Human-readable summary of the operation, e.g. "restart initiated for group 'backend'".
    pub message: String,
}

/// Paginated log query response for the `zaz_logs` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogsReport {
    /// Process name the query targeted (echoed back; "*" means all processes).
    pub name: String,
    /// Log entries on this page, oldest first.
    pub entries: Vec<LogEntry>,
    /// Total number of matching entries across all pages, if pagination metadata is available.
    pub total_count: Option<usize>,
    /// Whether more entries follow this page.
    pub has_more: Option<bool>,
    /// Offset used for this query.
    pub offset: Option<usize>,
}

/// One log line.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogEntry {
    /// Unix milliseconds when the line was captured.
    pub timestamp_ms: u64,
    /// Process name that produced the line.
    pub process: String,
    /// Group context, if available.
    pub group: Option<String>,
    /// Source of the line: process output or daemon-internal message.
    pub source: LogSourceReport,
    /// Stream the line came from when the source is a process.
    pub output_kind: OutputKindReport,
    /// The log content.
    pub content: String,
}

/// Origin of a log line.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogSourceReport {
    Process,
    Daemon,
}

/// Output stream kind for process logs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutputKindReport {
    Stdout,
    Stderr,
    Combined,
}

/// Parsed configuration response for the `zaz_config` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigReport {
    /// Absolute path of the loaded config file.
    pub path: String,
    pub settings: ConfigSettings,
    pub variables: BTreeMap<String, String>,
    pub groups: Vec<ConfigGroup>,
}

/// Global settings.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigSettings {
    pub shell: Option<String>,
    pub debounce_ms: u64,
    pub log_format: LogFormatReport,
}

/// Log output format.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogFormatReport {
    Pretty,
    Plain,
    Json,
}

/// One configured group.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigGroup {
    pub name: String,
    pub patterns: Vec<String>,
    pub ignore: Vec<String>,
    pub depends_on: Vec<String>,
    pub working_dir: Option<String>,
    pub env: BTreeMap<String, String>,
    pub tasks: Vec<ConfigTask>,
    pub daemons: Vec<ConfigDaemon>,
}

/// One configured task command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigTask {
    pub name: String,
    pub command: String,
    pub on_change_only: bool,
    pub silence: SilenceReport,
    pub working_dir: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// One configured daemon command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigDaemon {
    pub name: String,
    pub command: String,
    pub signal: SignalReport,
    pub no_pty: bool,
    pub silence: SilenceReport,
    pub working_dir: Option<String>,
    pub delay_ms: Option<u64>,
    pub env: BTreeMap<String, String>,
}

/// Output suppression level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SilenceReport {
    None,
    Stdout,
    Stderr,
    All,
}

/// Restart signal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum SignalReport {
    Sigterm,
    Sigint,
    Sighup,
    Sigkill,
    Sigquit,
    Sigusr1,
    Sigusr2,
}

impl From<&DaemonState> for StatusReport {
    fn from(state: &DaemonState) -> Self {
        Self {
            daemon_status: state.status.into(),
            watched_files: state.watched_files,
            last_change_ms: state.last_change,
            groups: state.groups.values().map(GroupReport::from).collect(),
        }
    }
}

impl From<DaemonStatus> for DaemonStatusReport {
    fn from(value: DaemonStatus) -> Self {
        match value {
            DaemonStatus::Starting => Self::Starting,
            DaemonStatus::Running => Self::Running,
            DaemonStatus::Stopping => Self::Stopping,
        }
    }
}

impl From<&GroupState> for GroupReport {
    fn from(group: &GroupState) -> Self {
        let mut processes = Vec::with_capacity(group.tasks.len() + group.daemons.len());
        processes.extend(
            group
                .tasks
                .iter()
                .map(|p| ProcessReport::from_state(p, ProcessKind::Task)),
        );
        processes.extend(
            group
                .daemons
                .iter()
                .map(|p| ProcessReport::from_state(p, ProcessKind::Daemon)),
        );
        Self {
            name: group.name.clone(),
            status: group.status.into(),
            processes,
        }
    }
}

impl ProcessReport {
    fn from_state(state: &ProcessState, kind: ProcessKind) -> Self {
        Self {
            name: state.name.clone(),
            kind,
            status: state.status.into(),
            pid: state.pid,
            exit_code: state.exit_code,
            duration_ms: state.duration_ms,
        }
    }
}

impl From<GroupStatus> for GroupStatusReport {
    fn from(value: GroupStatus) -> Self {
        match value {
            GroupStatus::Pending => Self::Pending,
            GroupStatus::Waiting => Self::Waiting,
            GroupStatus::Running => Self::Running,
            GroupStatus::Ready => Self::Ready,
            GroupStatus::Failed => Self::Failed,
            GroupStatus::Skipped => Self::Skipped,
        }
    }
}

impl From<ProcessStatus> for ProcessStatusReport {
    fn from(value: ProcessStatus) -> Self {
        match value {
            ProcessStatus::Pending => Self::Pending,
            ProcessStatus::Running => Self::Running,
            ProcessStatus::Success => Self::Success,
            ProcessStatus::Failed => Self::Failed,
            ProcessStatus::Backoff => Self::Backoff,
        }
    }
}

impl From<&DaemonState> for GroupsReport {
    fn from(state: &DaemonState) -> Self {
        Self {
            groups: state.groups.values().map(GroupSummary::from).collect(),
        }
    }
}

impl From<&GroupState> for GroupSummary {
    fn from(group: &GroupState) -> Self {
        Self {
            name: group.name.clone(),
            status: group.status.into(),
            task_count: group.tasks.len(),
            daemon_count: group.daemons.len(),
        }
    }
}

impl From<&LogLine> for LogEntry {
    fn from(line: &LogLine) -> Self {
        Self {
            timestamp_ms: line.timestamp,
            process: line.process.clone(),
            group: line.group.clone(),
            source: line.source.into(),
            output_kind: line.output_kind.into(),
            content: line.content.clone(),
        }
    }
}

impl From<LogSource> for LogSourceReport {
    fn from(value: LogSource) -> Self {
        match value {
            LogSource::Process => Self::Process,
            LogSource::Daemon => Self::Daemon,
        }
    }
}

impl From<OutputKind> for OutputKindReport {
    fn from(value: OutputKind) -> Self {
        match value {
            OutputKind::Stdout => Self::Stdout,
            OutputKind::Stderr => Self::Stderr,
            OutputKind::Combined => Self::Combined,
        }
    }
}

impl ConfigReport {
    /// Build a `ConfigReport` from a parsed config and the path it was loaded from.
    pub fn from_config(path: &std::path::Path, config: &Config) -> Self {
        Self {
            path: path.display().to_string(),
            settings: ConfigSettings::from(&config.settings),
            variables: config
                .variables
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            groups: config.groups.iter().map(ConfigGroup::from).collect(),
        }
    }
}

impl From<&Settings> for ConfigSettings {
    fn from(s: &Settings) -> Self {
        Self {
            shell: s.shell.clone(),
            debounce_ms: s.debounce_ms(),
            log_format: s.log_format.into(),
        }
    }
}

impl From<LogFormat> for LogFormatReport {
    fn from(value: LogFormat) -> Self {
        match value {
            LogFormat::Pretty => Self::Pretty,
            LogFormat::Plain => Self::Plain,
            LogFormat::Json => Self::Json,
        }
    }
}

impl From<&Group> for ConfigGroup {
    fn from(g: &Group) -> Self {
        Self {
            name: g.name.clone(),
            patterns: g.patterns.clone(),
            ignore: g.ignore.clone(),
            depends_on: g.depends_on.clone(),
            working_dir: g.working_dir.clone(),
            env: g.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            tasks: g.tasks.iter().map(ConfigTask::from).collect(),
            daemons: g.daemons.iter().map(ConfigDaemon::from).collect(),
        }
    }
}

impl From<&TaskCommand> for ConfigTask {
    fn from(t: &TaskCommand) -> Self {
        Self {
            name: t.name().to_string(),
            command: t.command.clone(),
            on_change_only: t.on_change_only,
            silence: t.silence.into(),
            working_dir: t.working_dir.clone(),
            env: t.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }
}

impl From<&DaemonCommand> for ConfigDaemon {
    fn from(d: &DaemonCommand) -> Self {
        Self {
            name: d.name().to_string(),
            command: d.command.clone(),
            signal: d.signal.into(),
            no_pty: d.no_pty,
            silence: d.silence.into(),
            working_dir: d.working_dir.clone(),
            delay_ms: d.delay_ms(),
            env: d.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }
}

impl From<Silence> for SilenceReport {
    fn from(value: Silence) -> Self {
        match value {
            Silence::None => Self::None,
            Silence::Stdout => Self::Stdout,
            Silence::Stderr => Self::Stderr,
            Silence::All => Self::All,
        }
    }
}

impl From<Signal> for SignalReport {
    fn from(value: Signal) -> Self {
        match value {
            Signal::Sigterm => Self::Sigterm,
            Signal::Sigint => Self::Sigint,
            Signal::Sighup => Self::Sighup,
            Signal::Sigkill => Self::Sigkill,
            Signal::Sigquit => Self::Sigquit,
            Signal::Sigusr1 => Self::Sigusr1,
            Signal::Sigusr2 => Self::Sigusr2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use std::path::PathBuf;
    use zaz_config::TaskCommand;

    fn sample_state() -> DaemonState {
        let mut groups = IndexMap::new();
        groups.insert(
            "backend".to_string(),
            GroupState {
                name: "backend".to_string(),
                status: GroupStatus::Ready,
                tasks: vec![ProcessState {
                    name: "build".to_string(),
                    status: ProcessStatus::Success,
                    pid: None,
                    exit_code: Some(0),
                    duration_ms: Some(420),
                }],
                daemons: vec![ProcessState {
                    name: "server".to_string(),
                    status: ProcessStatus::Running,
                    pid: Some(4242),
                    exit_code: None,
                    duration_ms: None,
                }],
            },
        );
        DaemonState {
            status: DaemonStatus::Running,
            groups,
            watched_files: 17,
            last_change: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn status_report_flattens_tasks_and_daemons() {
        let report: StatusReport = StatusReport::from(&sample_state());
        assert!(matches!(report.daemon_status, DaemonStatusReport::Running));
        assert_eq!(report.watched_files, 17);
        assert_eq!(report.last_change_ms, Some(1_700_000_000_000));
        assert_eq!(report.groups.len(), 1);
        let backend = &report.groups[0];
        assert_eq!(backend.name, "backend");
        assert_eq!(backend.processes.len(), 2);
        assert!(matches!(backend.processes[0].kind, ProcessKind::Task));
        assert_eq!(backend.processes[0].name, "build");
        assert!(matches!(backend.processes[1].kind, ProcessKind::Daemon));
        assert_eq!(backend.processes[1].pid, Some(4242));
    }

    #[test]
    fn groups_report_is_slim() {
        let report = GroupsReport::from(&sample_state());
        assert_eq!(report.groups.len(), 1);
        let summary = &report.groups[0];
        assert_eq!(summary.name, "backend");
        assert_eq!(summary.task_count, 1);
        assert_eq!(summary.daemon_count, 1);
    }

    #[test]
    fn log_entry_round_trips_log_line() {
        let line = LogLine::stderr("server", "boom").with_group("backend");
        let entry = LogEntry::from(&line);
        assert_eq!(entry.process, "server");
        assert_eq!(entry.group.as_deref(), Some("backend"));
        assert!(matches!(entry.source, LogSourceReport::Process));
        assert!(matches!(entry.output_kind, OutputKindReport::Stderr));
        assert_eq!(entry.content, "boom");
    }

    #[test]
    fn config_report_includes_path_and_groups() {
        let mut config = Config::default();
        config.groups.push(Group {
            name: "frontend".to_string(),
            patterns: vec!["src/**/*.ts".to_string()],
            tasks: vec![TaskCommand::new("typecheck", "tsc --noEmit")],
            ..Default::default()
        });
        let path = PathBuf::from("/tmp/zaz.toml");
        let report = ConfigReport::from_config(&path, &config);
        assert_eq!(report.path, "/tmp/zaz.toml");
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].name, "frontend");
        assert_eq!(report.groups[0].tasks.len(), 1);
        assert_eq!(report.groups[0].tasks[0].name, "typecheck");
        assert_eq!(report.groups[0].tasks[0].command, "tsc --noEmit");
    }

    #[test]
    fn schemas_are_generatable() {
        // Smoke test: make sure JsonSchema derives produce a schema for each top-level type.
        let _ = schemars::schema_for!(StatusReport);
        let _ = schemars::schema_for!(GroupsReport);
        let _ = schemars::schema_for!(LogsReport);
        let _ = schemars::schema_for!(LogsRequest);
        let _ = schemars::schema_for!(ConfigReport);
        let _ = schemars::schema_for!(RestartGroupRequest);
        let _ = schemars::schema_for!(RestartProcessRequest);
        let _ = schemars::schema_for!(MutationReport);
    }
}
