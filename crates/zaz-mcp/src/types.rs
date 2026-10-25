//! MCP-facing response types for zaz tools.
//!
//! These mirror the daemon and config types but are owned by the MCP crate so
//! the agent-facing schema stays stable across internal refactors and so
//! `schemars` derives stay contained here.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zaz_config::{
    Config, Group, LogFormat, ServiceCommand, Settings, Signal, Silence, TaskCommand,
};
use zaz_daemon::{
    DaemonState, DaemonStatus, EnvLevel, EnvReport, EnvSourceKind, EnvSourceReport, EnvStart,
    GroupEnvReport, GroupState, GroupStatus, LoadError, LogLine, LogSource, OutputKind,
    ProcessEnvReport, ProcessState, ProcessStatus,
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
    /// Why the daemon's first load failed, when it did. The daemon is `starting` with nothing
    /// built until the config is fixed and reloaded. A later reload failure never sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<LoadErrorReport>,
    /// Why the last refresh of a watched source failed, when it did. The daemon keeps running on
    /// the environment it already applied until a refresh succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_refresh_error: Option<LoadErrorReport>,
    /// Failed environment refreshes per workspace member, reported by a workspace supervisor.
    /// Each member is still running. `env_refresh_error` carries the first of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_refresh_errors: Vec<MemberErrorReport>,
    /// Errors per workspace member, reported by a workspace supervisor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_errors: Vec<MemberErrorReport>,
}

/// A coded failure to load a config.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoadErrorReport {
    /// Stable machine-readable code of the first error.
    pub code: String,
    /// Every error, one per line, each ending in its code.
    pub message: String,
    /// What to do next, when the first error carries a hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The error of one workspace member.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemberErrorReport {
    /// The member's project token, or its config path when it has none.
    pub member: String,
    pub error: LoadErrorReport,
}

/// Daemon lifecycle state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DaemonStatusReport {
    Starting,
    Running,
    Stopping,
}

/// State of a single group, with tasks and services collapsed into a uniform
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

/// State of a single managed process (task or service).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessReport {
    pub name: String,
    pub kind: ProcessKind,
    pub status: ProcessStatusReport,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
}

/// Whether a process is a task (run-to-completion) or a service (long-running).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProcessKind {
    Task,
    Service,
}

/// Process status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProcessStatusReport {
    Pending,
    Starting,
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
    pub service_count: usize,
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

/// Input parameters for the `zaz_env` tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EnvRequest {
    /// Project token selecting one workspace member. Required against a workspace supervisor;
    /// omit for a single-config daemon.
    #[serde(default)]
    pub project: Option<String>,
    /// Group to report. Omit for every group.
    #[serde(default)]
    pub group: Option<String>,
}

/// The loaded environment of the running config, for the `zaz_env` tool.
///
/// It holds key names and where each came from, never values. The error text of a failed source
/// can carry that command's stderr.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvToolReport {
    /// What this report describes. It reflects the running config, so an edit to `zaz.toml` or to
    /// a source that has not been reloaded or picked up by the watcher is not in it.
    pub scope: String,
    /// The workspace project this report describes, when the daemon is a workspace supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Overall lifecycle state of the daemon. `starting` with no groups means nothing is loaded.
    pub daemon_status: DaemonStatusReport,
    /// Why the first load failed, when it did. No keys are reported then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<LoadErrorReport>,
    /// Why the last refresh of a watched source failed, when it did. The groups show the
    /// environment the daemon is still running on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<LoadErrorReport>,
    /// Inherited variable names the base environment dropped, for chains that start from it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_inherited: Vec<String>,
    /// The groups, in declaration order.
    pub groups: Vec<EnvGroupReport>,
}

/// The environment sources of one group and its processes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvGroupReport {
    pub name: String,
    /// What the group's own environment starts from.
    pub starts_from: EnvStartReport,
    /// Sources that apply to the group, in the order they are applied.
    pub sources: Vec<EnvSourceInfo>,
    pub tasks: Vec<EnvProcessReport>,
    pub services: Vec<EnvProcessReport>,
}

/// The environment sources of one task or service.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvProcessReport {
    pub name: String,
    /// What the process's environment starts from.
    pub starts_from: EnvStartReport,
    /// Sources that apply to the process, in the order they are applied. They run from the
    /// settings level through the group's and then the process's own.
    pub sources: Vec<EnvSourceInfo>,
}

/// What a chain of sources is applied to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvStartReport {
    /// The daemon's own environment.
    Daemon,
    /// The fixed base environment plus any `env_passthrough` names.
    Base,
}

/// The config level a source was declared at.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvLevelReport {
    Settings,
    Group,
    Task,
    Service,
}

/// The kind of environment source.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvSourceKindReport {
    /// An `env_file`.
    File,
    /// An `env_command`.
    Command,
    /// An inline `env` table.
    Inline,
}

/// The key names one source set and unset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvSourceInfo {
    pub level: EnvLevelReport,
    /// The config field, such as `group 'api' env_file`.
    pub field: String,
    pub kind: EnvSourceKindReport,
    /// The `env_file` path or `env_command` as written in the config. Unset for an inline table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Names the source set.
    pub set: Vec<String>,
    /// Names the source removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset: Vec<String>,
    /// True when the source set and unset nothing. An `env_command` that prints nothing, such as
    /// `direnv export json` with an unallowed or missing `.envrc`, looks like this.
    pub produced_no_keys: bool,
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
    /// Process name. Matches the `name` field of a task or service entry inside the group.
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
    /// The `env_file` path as written. Loaded values are never shown.
    pub env_file: Option<String>,
    /// The `env_command` as written. Its output is never shown.
    pub env_command: Option<String>,
    /// Literal paths whose changes re-run the `env_command`, as written.
    #[serde(default)]
    pub env_watch: Vec<String>,
    /// Extra variable names, or `PREFIX_*` patterns, kept from the daemon's environment.
    pub env_passthrough: Vec<String>,
    /// The `env_command_timeout` in milliseconds, when set.
    pub env_command_timeout_ms: Option<u64>,
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
    pub env_file: Option<String>,
    pub env_command: Option<String>,
    /// Literal paths whose changes re-run the `env_command`, as written.
    #[serde(default)]
    pub env_watch: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub tasks: Vec<ConfigTask>,
    pub services: Vec<ConfigService>,
}

/// One configured task command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigTask {
    pub name: String,
    pub command: String,
    pub on_change_only: bool,
    pub silence: SilenceReport,
    pub working_dir: Option<String>,
    pub env_file: Option<String>,
    pub env_command: Option<String>,
    /// Literal paths whose changes re-run the `env_command`, as written.
    #[serde(default)]
    pub env_watch: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// One configured service command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigService {
    pub name: String,
    pub command: String,
    pub cleanup_command: Option<String>,
    pub ready_check: Option<String>,
    pub stop_command: Option<String>,
    pub kill_command: Option<String>,
    pub signal: SignalReport,
    pub no_pty: bool,
    pub silence: SilenceReport,
    pub working_dir: Option<String>,
    pub delay_ms: Option<u64>,
    pub stop_timeout_ms: Option<u64>,
    pub ready_poll_interval_ms: Option<u64>,
    pub ready_timeout_ms: Option<u64>,
    pub env_file: Option<String>,
    pub env_command: Option<String>,
    /// Literal paths whose changes re-run the `env_command`, as written.
    #[serde(default)]
    pub env_watch: Vec<String>,
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
            load_error: state.load_error.as_ref().map(LoadErrorReport::from),
            env_refresh_error: state.env_refresh_error.as_ref().map(LoadErrorReport::from),
            member_refresh_errors: state
                .member_refresh_errors
                .iter()
                .map(|m| MemberErrorReport {
                    member: m.member.clone(),
                    error: LoadErrorReport::from(&m.error),
                })
                .collect(),
            member_errors: state
                .member_errors
                .iter()
                .map(|m| MemberErrorReport {
                    member: m.member.clone(),
                    error: LoadErrorReport::from(&m.error),
                })
                .collect(),
        }
    }
}

impl EnvToolReport {
    /// Build the tool report from the daemon's, narrowed to `group` when one is named.
    ///
    /// Returns `None` when `group` names no group of a loaded config.
    pub fn from_daemon(report: &EnvReport, group: Option<&str>) -> Option<Self> {
        let groups: Vec<EnvGroupReport> = report
            .groups
            .iter()
            .filter(|candidate| group.map_or(true, |name| candidate.name == name))
            .map(EnvGroupReport::from)
            .collect();

        if group.is_some() && groups.is_empty() && !report.groups.is_empty() {
            return None;
        }

        Some(Self {
            scope: "Key names loaded by the running config. Values are never reported. Edits to \
                    zaz.toml or to a source that the daemon has not reloaded or picked up are not \
                    reflected."
                .to_string(),
            project: report.project.clone(),
            daemon_status: report.status.into(),
            load_error: report.load_error.as_ref().map(LoadErrorReport::from),
            refresh_error: report.refresh_error.as_ref().map(LoadErrorReport::from),
            dropped_inherited: report.dropped_inherited.clone(),
            groups,
        })
    }
}

impl From<&GroupEnvReport> for EnvGroupReport {
    fn from(group: &GroupEnvReport) -> Self {
        Self {
            name: group.name.clone(),
            starts_from: group.starts_from.into(),
            sources: group.sources.iter().map(EnvSourceInfo::from).collect(),
            tasks: group.tasks.iter().map(EnvProcessReport::from).collect(),
            services: group.services.iter().map(EnvProcessReport::from).collect(),
        }
    }
}

impl From<&ProcessEnvReport> for EnvProcessReport {
    fn from(process: &ProcessEnvReport) -> Self {
        Self {
            name: process.name.clone(),
            starts_from: process.starts_from.into(),
            sources: process.sources.iter().map(EnvSourceInfo::from).collect(),
        }
    }
}

impl From<&EnvSourceReport> for EnvSourceInfo {
    fn from(source: &EnvSourceReport) -> Self {
        Self {
            level: match source.level {
                EnvLevel::Settings => EnvLevelReport::Settings,
                EnvLevel::Group => EnvLevelReport::Group,
                EnvLevel::Task => EnvLevelReport::Task,
                EnvLevel::Service => EnvLevelReport::Service,
            },
            field: source.field.clone(),
            kind: match source.kind {
                EnvSourceKind::File => EnvSourceKindReport::File,
                EnvSourceKind::Command => EnvSourceKindReport::Command,
                EnvSourceKind::Inline => EnvSourceKindReport::Inline,
            },
            origin: source.origin.clone(),
            set: source.set.clone(),
            unset: source.unset.clone(),
            produced_no_keys: source.produced_no_keys,
        }
    }
}

impl From<EnvStart> for EnvStartReport {
    fn from(start: EnvStart) -> Self {
        match start {
            EnvStart::Daemon => Self::Daemon,
            EnvStart::Base => Self::Base,
        }
    }
}

impl From<&LoadError> for LoadErrorReport {
    fn from(error: &LoadError) -> Self {
        Self {
            code: error.code.clone(),
            message: error.message.clone(),
            hint: error.hint.clone(),
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
        let mut processes = Vec::with_capacity(group.tasks.len() + group.services.len());
        processes.extend(
            group
                .tasks
                .iter()
                .map(|p| ProcessReport::from_state(p, ProcessKind::Task)),
        );
        processes.extend(
            group
                .services
                .iter()
                .map(|p| ProcessReport::from_state(p, ProcessKind::Service)),
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
            ProcessStatus::Starting => Self::Starting,
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
            service_count: group.services.len(),
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
            env_file: s.env_file.clone(),
            env_command: s.env_command.clone(),
            env_watch: s.env_watch.clone(),
            env_passthrough: s.env_passthrough.clone(),
            env_command_timeout_ms: s.env_command_timeout.map(|t| t.as_millis()),
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
            env_file: g.env_file.clone(),
            env_command: g.env_command.clone(),
            env_watch: g.env_watch.clone(),
            env: g.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            tasks: g.tasks.iter().map(ConfigTask::from).collect(),
            services: g.services.iter().map(ConfigService::from).collect(),
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
            env_file: t.env_file.clone(),
            env_command: t.env_command.clone(),
            env_watch: t.env_watch.clone(),
            env: t.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }
}

impl From<&ServiceCommand> for ConfigService {
    fn from(d: &ServiceCommand) -> Self {
        Self {
            name: d.name().to_string(),
            command: d.command.clone(),
            cleanup_command: d.cleanup_command.clone(),
            ready_check: d.ready_check.clone(),
            stop_command: d.stop_command.clone(),
            kill_command: d.kill_command.clone(),
            signal: d.signal().into(),
            no_pty: d.no_pty,
            silence: d.silence.into(),
            working_dir: d.working_dir.clone(),
            env_file: d.env_file.clone(),
            env_command: d.env_command.clone(),
            env_watch: d.env_watch.clone(),
            delay_ms: d.delay_ms(),
            stop_timeout_ms: d.stop_timeout_ms(),
            ready_poll_interval_ms: d.ready_poll_interval_ms(),
            ready_timeout_ms: d.ready_timeout_ms(),
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
    use zaz_daemon::MemberError;

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
                services: vec![ProcessState {
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
            ..Default::default()
        }
    }

    #[test]
    fn status_report_carries_the_load_error_of_a_starting_daemon() {
        let state = DaemonState {
            status: DaemonStatus::Starting,
            load_error: Some(
                LoadError::new("env_file_missing", "no such file").with_hint("create it"),
            ),
            ..Default::default()
        };

        let report = StatusReport::from(&state);

        assert!(matches!(report.daemon_status, DaemonStatusReport::Starting));
        let error = report.load_error.expect("the load error is reported");
        assert_eq!(error.code, "env_file_missing");
        assert_eq!(error.hint.as_deref(), Some("create it"));
        assert!(report.member_errors.is_empty());
    }

    #[test]
    fn status_report_carries_every_member_refresh_failure() {
        let state = DaemonState {
            env_refresh_error: Some(LoadError::new("a", "member 'api': x")),
            member_refresh_errors: vec![
                MemberError {
                    member: "api".to_string(),
                    error: LoadError::new("a", "x"),
                },
                MemberError {
                    member: "web".to_string(),
                    error: LoadError::new("b", "y"),
                },
            ],
            ..Default::default()
        };

        let report = StatusReport::from(&state);

        assert!(report.env_refresh_error.is_some());
        assert_eq!(report.member_refresh_errors.len(), 2);
        assert_eq!(report.member_refresh_errors[1].member, "web");
    }

    #[test]
    fn status_report_carries_member_errors() {
        let state = DaemonState {
            member_errors: vec![MemberError {
                member: "/repo/web/zaz.toml".to_string(),
                error: LoadError::new("workspace_token_collision", "taken"),
            }],
            ..Default::default()
        };

        let report = StatusReport::from(&state);

        assert_eq!(report.member_errors.len(), 1);
        assert_eq!(report.member_errors[0].member, "/repo/web/zaz.toml");
        assert_eq!(
            report.member_errors[0].error.code,
            "workspace_token_collision"
        );
    }

    #[test]
    fn a_status_report_without_error_fields_still_deserializes() {
        let older =
            r#"{"daemon_status":"running","watched_files":0,"last_change_ms":null,"groups":[]}"#;

        let report: StatusReport = serde_json::from_str(older).unwrap();

        assert!(report.load_error.is_none());
    }

    #[test]
    fn status_report_flattens_tasks_and_services() {
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
        assert!(matches!(backend.processes[1].kind, ProcessKind::Service));
        assert_eq!(backend.processes[1].pid, Some(4242));
    }

    #[test]
    fn groups_report_is_slim() {
        let report = GroupsReport::from(&sample_state());
        assert_eq!(report.groups.len(), 1);
        let summary = &report.groups[0];
        assert_eq!(summary.name, "backend");
        assert_eq!(summary.task_count, 1);
        assert_eq!(summary.service_count, 1);
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
    fn config_report_shows_env_file_as_written_and_never_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("secrets.env");
        std::fs::write(&env_path, "TOKEN=hunter2\n").unwrap();

        let mut task = TaskCommand::new("build", "make");
        task.env_file = Some("task.env".to_string());
        task.env_command = Some("mise env --json".to_string());
        task.env_watch = vec!["mise.toml".to_string()];
        let mut service = ServiceCommand::new("web", "run");
        service.env_file = Some("svc.env".to_string());
        service.env_command = Some("dotenvx get --format json".to_string());

        let mut config = Config::default();
        config.settings.env_file = Some("secrets.env".to_string());
        config.settings.env_command = Some("direnv export json".to_string());
        config.settings.env_passthrough = vec!["OP_*".to_string()];
        config.settings.env_watch = vec![".envrc".to_string(), "../flake.nix".to_string()];
        config.settings.env_command_timeout = Some(zaz_config::HumanDuration::from_millis(30_000));
        config.groups.push(Group {
            name: "api".to_string(),
            env_file: Some("group.env".to_string()),
            env_command: Some("sops -d x.json".to_string()),
            tasks: vec![task],
            services: vec![service],
            ..Default::default()
        });

        let report = ConfigReport::from_config(&dir.path().join("zaz.toml"), &config);

        assert_eq!(report.settings.env_file.as_deref(), Some("secrets.env"));
        assert_eq!(
            report.settings.env_command.as_deref(),
            Some("direnv export json")
        );
        assert_eq!(report.settings.env_passthrough, vec!["OP_*".to_string()]);
        assert_eq!(report.settings.env_watch, [".envrc", "../flake.nix"]);
        assert_eq!(report.groups[0].tasks[0].env_watch, ["mise.toml"]);
        assert_eq!(report.settings.env_command_timeout_ms, Some(30_000));
        assert_eq!(
            report.groups[0].env_command.as_deref(),
            Some("sops -d x.json")
        );
        assert_eq!(
            report.groups[0].tasks[0].env_command.as_deref(),
            Some("mise env --json")
        );
        assert_eq!(
            report.groups[0].services[0].env_command.as_deref(),
            Some("dotenvx get --format json")
        );
        assert_eq!(report.groups[0].env_file.as_deref(), Some("group.env"));
        assert_eq!(
            report.groups[0].tasks[0].env_file.as_deref(),
            Some("task.env")
        );
        assert_eq!(
            report.groups[0].services[0].env_file.as_deref(),
            Some("svc.env")
        );

        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
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
