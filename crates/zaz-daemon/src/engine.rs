//! Core orchestration engine for zaz.
//!
//! The engine ties together configuration, file watching, and process management.

use crate::api::LogLine;
use crate::dependency::DependencyResolver;
use crate::log_storage::{LogQuery, LogQueryResult, LogStorage};
use crate::state::{
    DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus,
};
use crate::{ApiResponse, DaemonError};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use zaz_config::{Config, Group};
use zaz_process::{Daemon, Executor, OutputLine, TaskRunner};
use zaz_vars::Context;
use zaz_watch::{FileEvent, PatternSet, Watcher, WatcherConfig};

/// Completion signal from a spawned task execution.
#[derive(Debug)]
struct TaskCompletion {
    /// Unique task identifier ("group_name:task_name").
    task_id: String,
    /// Group name (for state updates).
    group_name: String,
    /// Task index in the group (for state updates).
    task_index: usize,
    /// Whether the task execution succeeded.
    success: bool,
    /// Final status.
    status: ProcessStatus,
    /// Duration in milliseconds.
    duration_ms: Option<u64>,
    /// Exit code.
    exit_code: Option<i32>,
}

/// Context for executing a single task in a spawned task.
#[derive(Clone)]
struct TaskExecutionContext {
    /// Unique task identifier ("group_name:task_name").
    task_id: String,
    /// Group name.
    group_name: String,
    /// Task name.
    task_name: String,
    /// Task index in the group.
    task_index: usize,
    /// Command to execute.
    command: String,
    /// Executor for running commands.
    executor: Executor,
    /// Log suppression level.
    silence: zaz_config::Silence,
}

/// Check if output should be suppressed based on silence setting and output kind.
fn should_suppress(silence: zaz_config::Silence, is_stderr: bool) -> bool {
    use zaz_config::Silence;
    match silence {
        Silence::None => false,
        Silence::All => true,
        Silence::Stdout => !is_stderr,
        Silence::Stderr => is_stderr,
    }
}

/// Execute a single task in a spawned task.
/// Sends logs via log_tx and returns completion result.
async fn execute_task(ctx: TaskExecutionContext, log_tx: mpsc::Sender<LogLine>) -> TaskCompletion {
    let task_runner = TaskRunner::new(ctx.executor);

    tracing::info!(task = %ctx.task_name, "running task");

    // Send "running" log
    let _ = log_tx
        .send(
            LogLine::daemon(&ctx.task_name, format!("running: {}", ctx.command))
                .with_group(ctx.group_name.clone()),
        )
        .await;

    let start = std::time::Instant::now();

    // Create channel for streaming output
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<OutputLine>();

    // Run command with streaming
    let command_future = task_runner.run_streaming(&ctx.command, output_tx);
    tokio::pin!(command_future);

    let result = loop {
        tokio::select! {
            biased;

            result = &mut command_future => {
                // Drain remaining output
                while let Some(line) = output_rx.recv().await {
                    let (content, is_stderr) = match line {
                        OutputLine::Stdout(s) => (s, false),
                        OutputLine::Stderr(s) => (s, true),
                    };
                    // Check silence setting before sending
                    if !should_suppress(ctx.silence, is_stderr) {
                        let log_line = if is_stderr {
                            LogLine::stderr(&ctx.task_name, content)
                        } else {
                            LogLine::stdout(&ctx.task_name, content)
                        };
                        let _ = log_tx
                            .send(log_line.with_group(ctx.group_name.clone()))
                            .await;
                    }
                }
                break result;
            }

            Some(line) = output_rx.recv() => {
                let (content, is_stderr) = match line {
                    OutputLine::Stdout(s) => (s, false),
                    OutputLine::Stderr(s) => (s, true),
                };
                // Check silence setting before sending
                if !should_suppress(ctx.silence, is_stderr) {
                    let log_line = if is_stderr {
                        LogLine::stderr(&ctx.task_name, content)
                    } else {
                        LogLine::stdout(&ctx.task_name, content)
                    };
                    let _ = log_tx
                        .send(log_line.with_group(ctx.group_name.clone()))
                        .await;
                }
            }
        }
    };

    let duration = start.elapsed();

    match result {
        Ok(output) => {
            let is_success = output.exit_code.map(|c| c == 0).unwrap_or(true);
            let status = if is_success {
                ProcessStatus::Success
            } else {
                ProcessStatus::Failed
            };

            // Send completion log
            let exit_code_str = output
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
            let log_msg = if is_success {
                format!(
                    "completed in {:.2}s (exit code: {})",
                    duration.as_secs_f64(),
                    exit_code_str
                )
            } else {
                format!(
                    "failed: process exited with status {}",
                    output.exit_code.unwrap_or(-1)
                )
            };
            let _ = log_tx
                .send(LogLine::daemon(&ctx.task_name, log_msg).with_group(ctx.group_name.clone()))
                .await;

            TaskCompletion {
                task_id: ctx.task_id,
                group_name: ctx.group_name,
                task_index: ctx.task_index,
                success: is_success,
                status,
                duration_ms: Some(duration.as_millis() as u64),
                exit_code: output.exit_code,
            }
        }
        Err(e) => {
            tracing::error!(task = %ctx.task_name, error = %e, "task execution failed");
            let _ = log_tx
                .send(
                    LogLine::daemon(&ctx.task_name, format!("failed: {}", e))
                        .with_group(ctx.group_name.clone()),
                )
                .await;

            TaskCompletion {
                task_id: ctx.task_id,
                group_name: ctx.group_name,
                task_index: ctx.task_index,
                success: false,
                status: ProcessStatus::Failed,
                duration_ms: Some(duration.as_millis() as u64),
                exit_code: None,
            }
        }
    }
}

// =============================================================================
// Trigger Types
// =============================================================================

/// Source of a trigger event that initiates group execution.
#[derive(Debug, Clone)]
pub enum TriggerSource {
    /// Initial daemon startup.
    Startup,
    /// File system change detected.
    FileChange {
        /// Files that changed.
        files: Vec<PathBuf>,
    },
    /// Task completed successfully, triggering dependents.
    TaskCompletion {
        /// Group whose task completed.
        group: String,
    },
    /// Daemon restarted, triggering dependent groups.
    DaemonRestart {
        /// Group whose daemon restarted.
        group: String,
    },
    /// Manual restart via API request.
    ManualRestart {
        /// Scope of the restart.
        scope: RestartScope,
    },
    /// Dependency became ready during startup.
    DependencyReady {
        /// Group that completed.
        completed_group: String,
    },
}

impl fmt::Display for TriggerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TriggerSource::Startup => write!(f, "startup"),
            TriggerSource::FileChange { files } => {
                write!(f, "file change ({} files)", files.len())
            }
            TriggerSource::TaskCompletion { group } => {
                write!(f, "task completion in '{}'", group)
            }
            TriggerSource::DaemonRestart { group } => {
                write!(f, "daemon restart in '{}'", group)
            }
            TriggerSource::ManualRestart { scope } => {
                write!(f, "manual restart ({})", scope)
            }
            TriggerSource::DependencyReady { completed_group } => {
                write!(f, "dependency '{}' ready", completed_group)
            }
        }
    }
}

/// Scope of a manual restart request.
#[derive(Debug, Clone)]
pub enum RestartScope {
    /// Restart a single group.
    Group(String),
    /// Restart a specific process within a group.
    Process {
        /// Group name.
        group: String,
        /// Process name (task or daemon).
        process: String,
    },
    /// Restart all groups.
    All,
}

impl fmt::Display for RestartScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestartScope::Group(name) => write!(f, "group '{}'", name),
            RestartScope::Process { group, process } => {
                write!(f, "process '{}' in group '{}'", process, group)
            }
            RestartScope::All => write!(f, "all groups"),
        }
    }
}

/// Lifecycle phase determines how triggers propagate to dependents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    /// Initial startup - groups go Pending -> Waiting -> Running -> Ready.
    /// Uses `trigger_waiting_dependents` to start groups waiting for dependencies.
    Startup,
    /// Runtime phase - groups already Ready, being re-triggered.
    /// Uses `cascade_restart_to_dependents` to restart dependent groups.
    Runtime,
}

/// Complete context for processing a trigger event.
///
/// This replaces the scattered `is_change_triggered` boolean parameter with
/// a more explicit and type-safe representation of trigger semantics.
#[derive(Debug, Clone)]
pub struct TriggerContext {
    /// The source of this trigger.
    pub source: TriggerSource,
    /// Variable expansion context for commands.
    pub vars: Context,
    /// Whether to run `on_change_only` tasks.
    ///
    /// - `true` for file change triggers (run all tasks including on_change_only)
    /// - `false` for startup, manual restarts, and dependency triggers
    pub run_on_change_tasks: bool,
    /// Whether to cascade/propagate to dependent groups after completion.
    pub should_cascade: bool,
}

/// The core orchestration engine.
pub struct Engine {
    /// Loaded configuration.
    config: Config,

    /// Path to the configuration file.
    config_path: PathBuf,

    /// File system watcher.
    watcher: Watcher,

    /// Pattern sets for each group.
    group_patterns: HashMap<String, PatternSet>,

    /// Managed groups with their state (ordered by config file order).
    groups: IndexMap<String, ManagedGroup>,

    /// Current daemon state (for status queries).
    state: DaemonState,

    /// Topologically sorted group names for dependency ordering.
    execution_order: Vec<String>,

    /// Broadcast channel for status updates (for streaming subscribers).
    status_tx: broadcast::Sender<ApiResponse>,

    /// Log storage with channel-based ingestion from spawned tasks.
    log_store: crate::log_store::LogStore,

    /// Currently running tasks (by "group:task" id). All tasks can run in parallel.
    running_tasks: std::collections::HashSet<String>,

    /// Tasks that need to re-run after their current execution completes.
    /// Maximum 1 pending per task (HashSet deduplicates).
    pending_tasks: std::collections::HashSet<String>,

    /// Dependency resolver for managing group dependencies and waiting state.
    dependency_resolver: DependencyResolver,

    /// Channel for receiving task completion signals from spawned tasks.
    task_completion_rx: mpsc::Receiver<TaskCompletion>,

    /// Sender for spawned tasks to signal task completion.
    task_completion_tx: mpsc::Sender<TaskCompletion>,

    /// Notification configuration from user config.
    notification_config: zaz_config::NotificationConfig,

    /// Whether the engine is running embedded in another process (e.g., TUI).
    /// When embedded, remote shutdown commands are rejected.
    embedded: bool,
}

/// Result of a config reload operation.
#[derive(Debug, Clone)]
pub enum ReloadResult {
    /// Reload succeeded with details about what changed.
    Success {
        added: Vec<String>,
        removed: Vec<String>,
        modified: Vec<String>,
    },
    /// Reload failed with an error message.
    Failed(String),
}

/// A managed watch group with its processes.
struct ManagedGroup {
    /// Group configuration.
    config: Group,

    /// Task executor.
    executor: Executor,

    /// Managed daemons.
    daemons: Vec<Daemon>,

    /// Current group state.
    state: GroupState,

    /// Whether daemons have been started at least once.
    /// Used to determine if we should start daemons (first time) or signal them (subsequent).
    daemons_started: bool,

    /// Pending restart times for daemons
    pending_restarts: Vec<Option<Instant>>,
}

impl Engine {
    /// Create a new engine from a configuration file.
    pub fn new(config_path: &Path) -> Result<Self, DaemonError> {
        Self::with_options(config_path, true, false)
    }

    /// Create a new engine for embedded mode (e.g., TUI).
    ///
    /// In embedded mode, remote shutdown commands are rejected.
    pub fn new_embedded(config_path: &Path) -> Result<Self, DaemonError> {
        Self::with_options(config_path, false, true)
    }

    /// Create a new engine with options.
    ///
    /// `verbose_output` controls whether process output is printed to stdout.
    /// `embedded` indicates whether the engine is running embedded in another process.
    pub fn with_options(
        config_path: &Path,
        verbose_output: bool,
        embedded: bool,
    ) -> Result<Self, DaemonError> {
        let config = zaz_config::load(config_path).map_err(DaemonError::Config)?;
        Self::from_config(config, config_path.to_path_buf(), verbose_output, embedded)
    }

    /// Create a new engine from a loaded configuration.
    pub fn from_config(
        config: Config,
        config_path: PathBuf,
        verbose_output: bool,
        embedded: bool,
    ) -> Result<Self, DaemonError> {
        // Load user config for notification settings
        let user_config = zaz_config::load_user_config();
        let notification_config = user_config.notifications;

        // Determine the config directory for variable expansion
        let config_dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        // Create watcher configuration
        let watcher_config = WatcherConfig {
            root: config_dir.clone(),
            debounce: config.settings.debounce.as_duration(),
            ..Default::default()
        };

        let mut watcher = Watcher::new(watcher_config).map_err(DaemonError::Watch)?;
        watcher.watch(&config_dir).map_err(DaemonError::Watch)?;

        // Build pattern sets and managed groups
        let mut group_patterns = HashMap::new();
        let mut groups = IndexMap::new();

        for group in &config.groups {
            // Create pattern set for this group
            let patterns =
                PatternSet::new(&group.patterns, &group.ignore).map_err(DaemonError::Watch)?;
            group_patterns.insert(group.name.clone(), patterns);

            // Create executor with shell, working directory, and group env
            // Default to config directory if no explicit working_dir is set
            let mut executor = Executor::new(config.settings.shell.clone());
            let working_dir = group
                .working_dir
                .clone()
                .unwrap_or_else(|| config_dir.to_string_lossy().to_string());
            executor = executor.with_working_dir(working_dir);
            if !group.env.is_empty() {
                executor = executor.with_env(group.env.clone());
            }

            // Create daemons with per-daemon working_dir and env overrides
            let daemons: Vec<Daemon> = group
                .daemons
                .iter()
                .map(|d| {
                    let mut daemon_executor = executor.clone();
                    if let Some(ref dir) = d.working_dir {
                        daemon_executor = daemon_executor.with_working_dir(dir.clone());
                    }
                    if !d.env.is_empty() {
                        daemon_executor = daemon_executor.extend_env(d.env.clone());
                    }
                    Daemon::new(d.clone(), daemon_executor)
                })
                .collect();

            // Initialize group state
            let state = GroupState {
                name: group.name.clone(),
                status: GroupStatus::Pending,
                tasks: group
                    .tasks
                    .iter()
                    .map(|t| ProcessState {
                        name: t.name().to_string(),
                        status: ProcessStatus::Pending,
                        ..Default::default()
                    })
                    .collect(),
                daemons: group
                    .daemons
                    .iter()
                    .map(|d| ProcessState {
                        name: d.name().to_string(),
                        status: ProcessStatus::Pending,
                        ..Default::default()
                    })
                    .collect(),
            };

            let daemon_count = daemons.len();
            groups.insert(
                group.name.clone(),
                ManagedGroup {
                    config: group.clone(),
                    executor,
                    daemons,
                    state,
                    daemons_started: false,
                    pending_restarts: vec![None; daemon_count],
                },
            );
        }

        // Compute execution order based on dependencies
        let execution_order = topological_sort(&config.groups)?;

        // Build dependency resolver from group configs
        let dependency_resolver = DependencyResolver::from_groups(
            config
                .groups
                .iter()
                .map(|g| (g.name.as_str(), g.depends_on.as_slice())),
        );

        let state = DaemonState {
            status: DaemonStatus::Starting,
            groups: groups
                .iter()
                .map(|(k, v)| (k.clone(), v.state.clone()))
                .collect(),
            watched_files: 0,
            last_change: None,
        };

        // Create broadcast channel for status updates (capacity 16)
        let (status_tx, _) = broadcast::channel(16);

        // Create log store with user config settings
        let log_store = {
            let mut store = crate::log_store::LogStore::new()
                .with_memory_limit(user_config.log_storage.memory_limit_bytes())
                .with_max_lines_per_process(user_config.log_storage.max_lines_per_process);

            if verbose_output {
                store = store.with_verbose_callback(|log| {
                    println!("[{}] {}", log.process, log.content);
                });
            }

            store
        };

        // Create mpsc channel for task completion signals
        let (task_completion_tx, task_completion_rx) = mpsc::channel(64);

        Ok(Self {
            config,
            config_path,
            watcher,
            group_patterns,
            groups,
            state,
            execution_order,
            status_tx,
            log_store,
            running_tasks: std::collections::HashSet::new(),
            pending_tasks: std::collections::HashSet::new(),
            dependency_resolver,
            task_completion_rx,
            task_completion_tx,
            notification_config,
            embedded,
        })
    }

    /// Get current daemon state.
    pub fn state(&self) -> &DaemonState {
        &self.state
    }

    /// Subscribe to status updates.
    pub fn subscribe(&self) -> broadcast::Receiver<ApiResponse> {
        self.status_tx.subscribe()
    }

    /// Subscribe to log stream.
    pub fn subscribe_logs(&self) -> broadcast::Receiver<LogLine> {
        self.log_store.subscribe()
    }

    /// Get a sender for submitting logs (for spawned tasks).
    pub fn log_sender(&self) -> mpsc::Sender<LogLine> {
        self.log_store.sender()
    }

    /// Add a log line to storage and broadcast.
    pub fn push_log(&mut self, log: LogLine) {
        self.log_store.push(log);
    }

    /// Get stored logs for a process.
    ///
    /// If `name` is "*", returns logs from all processes sorted by timestamp.
    pub fn get_logs(&self, name: &str, limit: Option<usize>) -> Vec<LogLine> {
        self.log_store.get(name, limit)
    }

    /// Query logs with pagination and filtering support.
    ///
    /// This is the new API for log retrieval that supports pagination.
    pub fn query_logs(&self, query: LogQuery) -> LogQueryResult {
        self.log_store.query(query)
    }

    /// Spawn a background task to read PTY output and push to logs.
    fn spawn_pty_reader(
        &self,
        process: String,
        group: Option<String>,
        reader: Box<dyn std::io::Read + Send>,
    ) {
        use std::io::BufRead;

        let log_tx = self.log_store.sender();
        tokio::task::spawn_blocking(move || {
            let mut buf_reader = std::io::BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                match buf_reader.read_line(&mut line) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        // Trim trailing newline
                        let content = line.trim_end().to_string();
                        if !content.is_empty() {
                            let mut log_line = LogLine::process(&process, content);
                            if let Some(ref g) = group {
                                log_line = log_line.with_group(g.clone());
                            }
                            // Send to engine for storage and broadcast
                            if log_tx.blocking_send(log_line).is_err() {
                                // Channel closed, engine is shutting down
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            process = %process,
                            error = %e,
                            "PTY read error"
                        );
                        break;
                    }
                }
            }

            tracing::debug!(process = %process, "PTY reader finished");
        });
    }

    /// Process incoming logs from spawned tasks.
    ///
    /// This MUST be called before handling API requests to ensure fresh logs
    /// are visible. Also call periodically in the main loop.
    pub fn process_incoming_logs(&mut self) {
        self.log_store.drain();
    }

    /// Run the initial startup sequence.
    ///
    /// This runs all tasks (respecting on_change_only) and starts all daemons.
    /// Groups with no tasks (or only on_change_only tasks) will have their daemons
    /// started immediately. Groups with tasks will have daemons started after
    /// all tasks complete successfully.
    pub async fn startup(&mut self) -> Result<(), DaemonError> {
        tracing::info!("starting initial run");
        self.state.status = DaemonStatus::Running;

        let trigger_ctx = self.startup_context();

        // Track groups that need immediate daemon startup (no runnable tasks at startup
        // AND all dependencies satisfied)
        let mut groups_needing_daemon_start: Vec<String> = Vec::new();

        for group_name in &self.execution_order.clone() {
            // Check if group has unready dependencies using the resolver
            if let Some(unready_deps) = self.dependency_resolver.mark_waiting(group_name) {
                // Has unready dependencies - mark as Waiting
                tracing::debug!(
                    group = %group_name,
                    waiting_for = ?unready_deps,
                    "group waiting for dependencies"
                );
                // Sync Engine's status
                if let Some(group) = self.groups.get_mut(group_name) {
                    group.state.status = GroupStatus::Waiting;
                }
                continue;
            }

            // Can start group when all dependencies are Ready or no dependencies
            let has_startup_tasks = self
                .groups
                .get(group_name)
                .map(|g| g.config.tasks.iter().any(|t| !t.on_change_only))
                .unwrap_or(false);

            if has_startup_tasks {
                // Spawn tasks; daemons will start when tasks complete
                self.spawn_group_tasks(group_name, &trigger_ctx);
            } else {
                // Group has no tasks: queue daemon startup
                let has_daemons = self
                    .groups
                    .get(group_name)
                    .map(|g| !g.daemons.is_empty())
                    .unwrap_or(false);

                if has_daemons {
                    groups_needing_daemon_start.push(group_name.clone());
                } else {
                    // No tasks and no daemons - mark as Ready immediately
                    // This is important for groups that only serve as dependency markers
                    self.set_group_status(group_name, GroupStatus::Ready);
                }
            }
        }

        // Start daemons for groups with no tasks (and trigger their dependents)
        for group_name in groups_needing_daemon_start {
            if let Err(e) = self.handle_daemon_action(&group_name, true).await {
                tracing::error!(group = %group_name, error = %e, "failed to start daemons");
            }

            // trigger_dependents will be called after daemon action completes
            // via process_task_completions or here for groups without tasks
            self.trigger_dependents(&group_name);
        }

        self.update_state();
        Ok(())
    }

    /// Run a single group's tasks and start its daemons.
    async fn run_group(
        &mut self,
        group_name: &str,
        changed_files: &[PathBuf],
        is_change_triggered: bool,
    ) -> Result<(), DaemonError> {
        // Extract to avoid borrow issues
        let (executor, tasks, group_exists) = {
            let Some(group) = self.groups.get_mut(group_name) else {
                return Err(DaemonError::GroupNotFound(group_name.to_string()));
            };
            group.state.status = GroupStatus::Running;
            (group.executor.clone(), group.config.tasks.clone(), true)
        };

        if !group_exists {
            return Err(DaemonError::GroupNotFound(group_name.to_string()));
        }

        tracing::info!(group = group_name, "running group");
        self.update_state();

        // Build variable context
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_files(changed_files.to_vec())
            .with_root(config_dir.to_path_buf());

        // Run tasks
        let task_runner = TaskRunner::new(executor);
        for (idx, task) in tasks.iter().enumerate() {
            // Skip on_change_only tasks during initial startup
            if task.on_change_only && !is_change_triggered {
                tracing::debug!(task = %task.name(), "skipping on_change_only task during startup");
                continue;
            }

            // Expand variables in command
            let expander = zaz_vars::Expander::new(&context);
            let command = expander
                .expand(&task.command)
                .map_err(|e| DaemonError::VarExpansion(e.to_string()))?;

            tracing::info!(task = %task.name(), "running task");

            // Update state
            if let Some(group) = self.groups.get_mut(group_name) {
                group.state.tasks[idx].status = ProcessStatus::Running;
            }
            self.update_state();

            let start = std::time::Instant::now();
            self.push_log(
                LogLine::daemon(task.name(), format!("running: {}", command))
                    .with_group(group_name.to_string()),
            );

            let task_name = task.name().to_string();
            let group_name_owned = group_name.to_string();

            // Create channel for streaming output
            let (output_tx, mut output_rx) = mpsc::unbounded_channel::<OutputLine>();

            // Run command directly (no spawn) - use select to stream output in real-time
            tracing::debug!(task = %task_name, "starting streaming command");
            let command_future = task_runner.run_streaming(&command, output_tx);
            tokio::pin!(command_future);

            let inner_result = loop {
                tokio::select! {
                    biased; // Check in order: command completion first

                    result = &mut command_future => {
                        tracing::debug!(task = %task_name, "command completed, draining output");
                        // Command completed - drain any remaining output
                        while let Some(line) = output_rx.recv().await {
                            let content = match line {
                                OutputLine::Stdout(s) => s,
                                OutputLine::Stderr(s) => s,
                            };
                            self.push_log(
                                LogLine::process(&task_name, content)
                                    .with_group(group_name_owned.clone()),
                            );
                        }
                        tracing::debug!(task = %task_name, "output drained");
                        break result;
                    }

                    Some(line) = output_rx.recv() => {
                        let content = match line {
                            OutputLine::Stdout(s) => s,
                            OutputLine::Stderr(s) => s,
                        };
                        self.push_log(
                            LogLine::process(&task_name, content)
                                .with_group(group_name_owned.clone()),
                        );
                    }
                }
            };

            match inner_result {
                Ok(output) => {
                    let duration = start.elapsed();

                    let is_success = output.exit_code.map(|c| c == 0).unwrap_or(true);
                    if is_success {
                        // Push daemon log for task completion
                        let exit_code_str = output
                            .exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        self.push_log(
                            LogLine::daemon(
                                &task_name,
                                format!(
                                    "completed in {:.2}s (exit code: {})",
                                    duration.as_secs_f64(),
                                    exit_code_str
                                ),
                            )
                            .with_group(group_name_owned.clone()),
                        );

                        tracing::info!(
                            task = %task_name,
                            duration_ms = duration.as_millis(),
                            "task completed"
                        );
                        if let Some(group) = self.groups.get_mut(group_name) {
                            group.state.tasks[idx].status = ProcessStatus::Success;
                            group.state.tasks[idx].duration_ms = Some(duration.as_millis() as u64);
                            group.state.tasks[idx].exit_code = output.exit_code;
                        }

                        // Send notification for task success
                        crate::notify::send_notification(
                            &self.notification_config,
                            crate::notify::NotifyEvent::task_success(
                                &task_name,
                                group_name,
                                duration.as_millis() as u64,
                            ),
                        );
                    } else {
                        let exit_code = output.exit_code.unwrap_or(-1);
                        // Push daemon log for task failure
                        self.push_log(
                            LogLine::daemon(
                                &task_name,
                                format!("failed: process exited with status {}", exit_code),
                            )
                            .with_group(group_name_owned.clone()),
                        );

                        tracing::error!(
                            task = %task_name,
                            exit_code = exit_code,
                            "task failed"
                        );
                        if let Some(group) = self.groups.get_mut(group_name) {
                            group.state.tasks[idx].status = ProcessStatus::Failed;
                            group.state.tasks[idx].exit_code = output.exit_code;
                            group.state.status = GroupStatus::Failed;
                        }

                        // Send notification for task failure
                        crate::notify::send_notification(
                            &self.notification_config,
                            crate::notify::NotifyEvent::task_failed(
                                &task_name,
                                group_name,
                                output.exit_code,
                            ),
                        );

                        self.update_state();
                        return Err(DaemonError::TaskFailed {
                            task: task_name,
                            error: format!("process exited with status {}", exit_code),
                        });
                    }
                }
                Err(e) => {
                    // Push daemon log for spawn/system failure (no output captured)
                    self.push_log(
                        LogLine::daemon(&task_name, format!("failed: {}", e))
                            .with_group(group_name_owned),
                    );

                    tracing::error!(task = %task_name, error = %e, "task failed");
                    if let Some(group) = self.groups.get_mut(group_name) {
                        group.state.tasks[idx].status = ProcessStatus::Failed;
                        group.state.status = GroupStatus::Failed;
                    }

                    // Send notification for task failure (spawn error)
                    crate::notify::send_notification(
                        &self.notification_config,
                        crate::notify::NotifyEvent::task_failed(&task_name, group_name, None),
                    );

                    self.update_state();
                    return Err(DaemonError::TaskFailed {
                        task: task_name,
                        error: e.to_string(),
                    });
                }
            }
        }

        // Start or restart daemons
        // Collect daemon names first to avoid borrow conflicts
        let daemon_names: Vec<String> = self
            .groups
            .get(group_name)
            .map(|g| g.daemons.iter().map(|d| d.name().to_string()).collect())
            .unwrap_or_default();

        for daemon_name in &daemon_names {
            if is_change_triggered {
                self.push_log(
                    LogLine::daemon(daemon_name, "restarting").with_group(group_name.to_string()),
                );
            } else {
                self.push_log(
                    LogLine::daemon(daemon_name, "starting").with_group(group_name.to_string()),
                );
            }
        }

        // Collect PTY readers for newly started daemons
        let mut pty_readers: Vec<(String, Option<String>, Box<dyn std::io::Read + Send>)> =
            Vec::new();

        if let Some(group) = self.groups.get_mut(group_name) {
            for (idx, daemon) in group.daemons.iter_mut().enumerate() {
                if is_change_triggered {
                    // Signal existing daemon to restart
                    tracing::info!(daemon = %daemon.name(), "signaling daemon restart");
                    daemon.signal_restart().map_err(DaemonError::Process)?;
                } else {
                    // Apply startup delay if configured
                    if let Some(delay) = daemon.startup_delay() {
                        tracing::info!(
                            daemon = %daemon.name(),
                            delay_ms = delay.as_millis(),
                            "waiting before starting daemon"
                        );
                        tokio::time::sleep(delay).await;
                    }

                    // Start daemon for the first time
                    tracing::info!(daemon = %daemon.name(), "starting daemon");
                    daemon.start().map_err(DaemonError::Process)?;

                    // Get PTY reader for streaming output
                    if let Some(reader) = daemon.try_clone_reader() {
                        pty_readers.push((
                            daemon.name().to_string(),
                            Some(group_name.to_string()),
                            reader,
                        ));
                    }
                }

                group.state.daemons[idx].status = ProcessStatus::Running;
                group.state.daemons[idx].pid = daemon.pid();
            }

            group.state.status = GroupStatus::Ready;
        }

        // Spawn PTY reader tasks (outside the mutable borrow)
        for (process, group, reader) in pty_readers {
            self.spawn_pty_reader(process, group, reader);
        }

        // Cascade restart to dependent groups (only for change-triggered restarts)
        if is_change_triggered {
            if let Err(e) = self.cascade_daemon_restart(group_name).await {
                tracing::error!(
                    group = %group_name,
                    error = %e,
                    "failed to cascade daemon restart"
                );
            }
        }

        self.update_state();
        Ok(())
    }

    /// Process file change events.
    ///
    /// All tasks run in parallel. Same task cannot run concurrently - it gets queued.
    pub async fn handle_changes(&mut self, events: Vec<FileEvent>) -> Result<(), DaemonError> {
        if events.is_empty() {
            return Ok(());
        }

        let changed_paths: Vec<PathBuf> = events.iter().map(|e| e.path.clone()).collect();

        self.state.last_change = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        );

        // Determine which groups are affected
        let mut affected_groups = Vec::new();
        for (name, patterns) in &self.group_patterns {
            if changed_paths.iter().any(|p| patterns.matches(p)) {
                affected_groups.push(name.clone());
            }
        }

        if affected_groups.is_empty() {
            tracing::debug!("no groups affected by changes");
            return Ok(());
        }

        // Dedupe affected groups to only root groups, because dependents get
        // triggered through cascade
        let roots = filter_affected_to_roots(&affected_groups, |g| self.get_group_dependencies(g));

        tracing::info!(
            files = changed_paths.len(),
            affected = ?affected_groups,
            roots = ?roots,
            "processing file changes (dependents will cascade)"
        );

        // Build trigger context for command expansion
        let trigger_ctx = self.file_change_context(changed_paths);

        for group_name in roots {
            self.spawn_group_tasks(&group_name, &trigger_ctx);
        }

        Ok(())
    }

    /// Spawn all tasks in a group. Each task runs in parallel with per-task deduplication.
    fn spawn_group_tasks(&mut self, group_name: &str, trigger_ctx: &TriggerContext) {
        tracing::debug!(
            group = %group_name,
            trigger_source = ?trigger_ctx.source,
            run_on_change_tasks = trigger_ctx.run_on_change_tasks,
            should_cascade = trigger_ctx.should_cascade,
            "spawning group tasks"
        );

        // Get group configuration
        let (tasks, executor) = {
            let Some(group) = self.groups.get_mut(group_name) else {
                tracing::warn!(group = %group_name, "group not found");
                return;
            };
            group.state.status = GroupStatus::Running;
            (group.config.tasks.clone(), group.executor.clone())
        };

        // Spawn each task independently (parallel execution)
        for (idx, task) in tasks.iter().enumerate() {
            // Skip on_change_only tasks unless this is a change-triggered run
            if task.on_change_only && !trigger_ctx.run_on_change_tasks {
                tracing::debug!(task = %task.name(), "skipping on_change_only task");
                continue;
            }

            // Expand variables in command
            let expander = zaz_vars::Expander::new(&trigger_ctx.vars);
            let command = match expander.expand(&task.command) {
                Ok(cmd) => cmd,
                Err(e) => {
                    tracing::error!(task = %task.name(), error = %e, "variable expansion failed");
                    continue;
                }
            };

            // Use task-specific working_dir and env if set
            let mut task_executor = executor.clone();
            if let Some(ref dir) = task.working_dir {
                task_executor = task_executor.with_working_dir(dir.clone());
            }
            if !task.env.is_empty() {
                task_executor = task_executor.extend_env(task.env.clone());
            }

            self.spawn_task(
                group_name,
                task.name(),
                idx,
                command,
                task_executor,
                task.silence,
            );
        }

        self.update_state();
    }

    /// Spawn a single task if not already running.
    /// If already running, adds to pending_tasks for later execution.
    fn spawn_task(
        &mut self,
        group_name: &str,
        task_name: &str,
        task_index: usize,
        command: String,
        executor: Executor,
        silence: zaz_config::Silence,
    ) {
        let task_id = format!("{}:{}", group_name, task_name);

        // Check if this task is already running
        if self.running_tasks.contains(&task_id) {
            tracing::debug!(task_id = %task_id, "task already running, queuing for later");
            self.pending_tasks.insert(task_id);
            return;
        }

        // Mark task as running
        self.running_tasks.insert(task_id.clone());
        if let Some(group) = self.groups.get_mut(group_name) {
            if task_index < group.state.tasks.len() {
                group.state.tasks[task_index].status = ProcessStatus::Running;
            }
        }

        let ctx = TaskExecutionContext {
            task_id,
            group_name: group_name.to_string(),
            task_name: task_name.to_string(),
            task_index,
            command,
            executor,
            silence,
        };

        let log_tx = self.log_store.sender();
        let completion_tx = self.task_completion_tx.clone();

        tracing::info!(task = %task_name, group = %group_name, "spawning task execution");

        // Spawn the task execution
        tokio::spawn(async move {
            let completion = execute_task(ctx, log_tx).await;
            // Send completion signal (ignore error if receiver dropped)
            let _ = completion_tx.send(completion).await;
        });
    }

    /// Process completed task executions and handle pending re-runs.
    ///
    /// When all tasks in a group complete successfully:
    /// - If daemons haven't been started yet, start them
    /// - If daemons are already running, signal them to restart
    pub async fn process_task_completions(&mut self) {
        // Collect groups that need daemon action (group_name, should_start_not_signal)
        let mut daemon_actions: Vec<(String, bool)> = Vec::new();
        // Collect groups that failed (for cascade_skip)
        let mut failed_groups: Vec<String> = Vec::new();

        while let Ok(completion) = self.task_completion_rx.try_recv() {
            // Handle special __daemon_start__ task for groups without tasks
            if completion.task_id == "__daemon_start__" {
                tracing::debug!(
                    group = %completion.group_name,
                    "processing daemon start request for group without tasks"
                );
                daemon_actions.push((completion.group_name.clone(), true));
                continue;
            }

            tracing::info!(
                task_id = %completion.task_id,
                success = %completion.success,
                "task execution completed"
            );

            // Remove from running set first (needed for group status check)
            self.running_tasks.remove(&completion.task_id);

            // Extract task name from task_id for notifications
            let task_name = completion
                .task_id
                .split_once(':')
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| completion.task_id.clone());

            // Send notification for individual task completion
            if completion.success {
                crate::notify::send_notification(
                    &self.notification_config,
                    crate::notify::NotifyEvent::task_success(
                        &task_name,
                        &completion.group_name,
                        completion.duration_ms.unwrap_or(0),
                    ),
                );
            } else {
                crate::notify::send_notification(
                    &self.notification_config,
                    crate::notify::NotifyEvent::task_failed(
                        &task_name,
                        &completion.group_name,
                        completion.exit_code,
                    ),
                );
            }

            // Update task state and recalculate group status
            if let Some(group) = self.groups.get_mut(&completion.group_name) {
                if completion.task_index < group.state.tasks.len() {
                    group.state.tasks[completion.task_index].status = completion.status;
                    group.state.tasks[completion.task_index].duration_ms = completion.duration_ms;
                    group.state.tasks[completion.task_index].exit_code = completion.exit_code;
                }

                // Check if any tasks from this group are still running
                let group_prefix = format!("{}:", completion.group_name);
                let has_running_tasks = self
                    .running_tasks
                    .iter()
                    .any(|id| id.starts_with(&group_prefix));

                if let Some(new_status) =
                    calculate_group_status_from_tasks(&group.state.tasks, has_running_tasks)
                {
                    // All tasks complete - update group status
                    group.state.status = new_status;
                    let any_failed = new_status == GroupStatus::Failed;

                    // Send notification for group completion
                    if any_failed {
                        crate::notify::send_notification(
                            &self.notification_config,
                            crate::notify::NotifyEvent::group_failed(&completion.group_name),
                        );
                        // Queue cascade_skip for this failed group
                        failed_groups.push(completion.group_name.clone());
                    } else {
                        crate::notify::send_notification(
                            &self.notification_config,
                            crate::notify::NotifyEvent::group_complete(&completion.group_name),
                        );

                        // Queue daemon action: start if not yet started, signal if already running
                        if !group.daemons.is_empty() {
                            let should_start = !group.daemons_started;
                            daemon_actions.push((completion.group_name.clone(), should_start));
                        } else {
                            // No daemons - group is ready, daemon_actions will trigger dependents
                            // We still need to add to daemon_actions for trigger_dependents call
                            daemon_actions.push((completion.group_name.clone(), true));
                        }
                    }
                }
            }

            // Check if this task is pending re-run
            if self.pending_tasks.remove(&completion.task_id) {
                tracing::info!(task_id = %completion.task_id, "re-running pending task");
                // Parse task_id back to group:task
                if let Some((group_name, task_name)) = completion.task_id.split_once(':') {
                    // Get executor and command for re-run
                    if let Some(group) = self.groups.get(group_name) {
                        if let Some(task) =
                            group.config.tasks.iter().find(|t| t.name() == task_name)
                        {
                            // For re-run, we use empty context (no specific changed files)
                            let context = Context::new()
                                .with_variables(self.config.variables.clone())
                                .with_root(
                                    self.config_path
                                        .parent()
                                        .unwrap_or(Path::new("."))
                                        .to_path_buf(),
                                );
                            let expander = zaz_vars::Expander::new(&context);
                            if let Ok(command) = expander.expand(&task.command) {
                                // Use task-specific working_dir and env if set
                                let mut task_executor = group.executor.clone();
                                if let Some(ref dir) = task.working_dir {
                                    task_executor = task_executor.with_working_dir(dir.clone());
                                }
                                if !task.env.is_empty() {
                                    task_executor = task_executor.extend_env(task.env.clone());
                                }
                                self.spawn_task(
                                    group_name,
                                    task_name,
                                    completion.task_index,
                                    command,
                                    task_executor,
                                    task.silence,
                                );
                            }
                        }
                    }
                }
            }

            self.update_state();
        }

        // Process cascade_skip for failed groups
        for group_name in failed_groups {
            self.cascade_skip(&group_name);
        }

        // Process daemon actions after releasing borrows from the loop
        for (group_name, should_start) in daemon_actions {
            // Check if this group has daemons to start/signal
            let has_daemons = self
                .groups
                .get(&group_name)
                .map(|g| !g.daemons.is_empty())
                .unwrap_or(false);

            if has_daemons {
                if let Err(e) = self.handle_daemon_action(&group_name, should_start).await {
                    tracing::error!(group = %group_name, error = %e, "failed to handle daemon action");
                }
            }

            // Propagate completion to dependents based on lifecycle phase.
            // - Startup: triggers groups that were waiting for dependencies
            // - Runtime: cascades restart to already-Ready dependent groups
            let phase = self.determine_lifecycle_phase(&group_name);
            let trigger_ctx = self.task_completion_context(&group_name);
            tracing::debug!(
                group = %group_name,
                phase = ?phase,
                trigger_source = ?trigger_ctx.source,
                should_cascade = trigger_ctx.should_cascade,
                "propagating task completion to dependents"
            );
            if trigger_ctx.should_cascade {
                if let Err(e) = self.propagate_to_dependents(&group_name, phase).await {
                    tracing::error!(
                        group = %group_name,
                        error = %e,
                        "failed to propagate to dependents"
                    );
                }
            }
        }

        self.update_state();
    }

    /// Handle daemon startup or signal for a group after successful task completion.
    ///
    /// - If `should_start` is true, starts daemons (first time)
    /// - If `should_start` is false, signals daemons to restart (subsequent times)
    async fn handle_daemon_action(
        &mut self,
        group_name: &str,
        should_start: bool,
    ) -> Result<(), DaemonError> {
        // Collect daemon names for logging
        let daemon_names: Vec<String> = self
            .groups
            .get(group_name)
            .map(|g| g.daemons.iter().map(|d| d.name().to_string()).collect())
            .unwrap_or_default();

        // Log what we're about to do
        for daemon_name in &daemon_names {
            if should_start {
                self.push_log(
                    LogLine::daemon(daemon_name, "starting").with_group(group_name.to_string()),
                );
            } else {
                self.push_log(
                    LogLine::daemon(daemon_name, "restarting").with_group(group_name.to_string()),
                );
            }
        }

        // Collect PTY readers for newly started daemons
        let mut pty_readers: Vec<(String, Option<String>, Box<dyn std::io::Read + Send>)> =
            Vec::new();

        if let Some(group) = self.groups.get_mut(group_name) {
            for (idx, daemon) in group.daemons.iter_mut().enumerate() {
                // Check if daemon is actually running before deciding what to do
                let is_running = daemon.is_running();

                if should_start || !is_running {
                    // Start daemon if:
                    // - This is the first time (should_start=true), OR
                    // - The daemon is not running (crashed before file change)
                    if !should_start && !is_running {
                        tracing::info!(
                            daemon = %daemon.name(),
                            "daemon not running, starting instead of signaling"
                        );
                    }

                    // Apply startup delay if configured
                    if let Some(delay) = daemon.startup_delay() {
                        tracing::info!(
                            daemon = %daemon.name(),
                            delay_ms = delay.as_millis(),
                            "waiting before starting daemon"
                        );
                        tokio::time::sleep(delay).await;
                    }

                    // Start daemon
                    tracing::info!(daemon = %daemon.name(), "starting daemon");
                    daemon.start().map_err(DaemonError::Process)?;

                    // Get PTY reader for streaming output
                    if let Some(reader) = daemon.try_clone_reader() {
                        pty_readers.push((
                            daemon.name().to_string(),
                            Some(group_name.to_string()),
                            reader,
                        ));
                    }
                } else {
                    // Signal existing daemon to restart
                    tracing::info!(daemon = %daemon.name(), "signaling daemon restart");
                    daemon.signal_restart().map_err(DaemonError::Process)?;
                }

                group.state.daemons[idx].status = ProcessStatus::Running;
                group.state.daemons[idx].pid = daemon.pid();
            }

            group.daemons_started = true;
            group.state.status = GroupStatus::Ready;
        }

        // Spawn PTY reader tasks (outside the mutable borrow)
        for (process, group, reader) in pty_readers {
            self.spawn_pty_reader(process, group, reader);
        }

        // Note: Cascade to dependents is handled by the caller via propagate_to_dependents()
        // This keeps cascade logic centralized.

        self.update_state();
        Ok(())
    }

    /// Wait for all running tasks to complete.
    ///
    /// This polls for task completions until no tasks are running.
    pub async fn wait_for_tasks(&mut self) {
        while !self.running_tasks.is_empty() {
            self.process_task_completions().await;
            if self.running_tasks.is_empty() {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Final drain of any remaining completions
        self.process_task_completions().await;
    }

    /// Poll for file changes and process them.
    pub async fn poll(&mut self) -> Result<bool, DaemonError> {
        // Process any completed tasks first
        self.process_task_completions().await;

        // Create a combined pattern set for polling
        let combined = self.combined_patterns()?;

        if let Some(events) = self.watcher.poll(&combined) {
            self.handle_changes(events).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Check daemon processes and handle restarts.
    ///
    /// This function is non-blocking: when a daemon exits, it schedules a restart
    /// for later rather than sleeping. This allows the main loop to remain responsive
    /// to API commands while waiting for restart delays.
    pub async fn check_daemons(&mut self) -> Result<(), DaemonError> {
        let mut pty_readers: Vec<(String, Option<String>, Box<dyn std::io::Read + Send>)> =
            Vec::new();

        let now = Instant::now();
        for (group_name, group) in self.groups.iter_mut() {
            for (idx, daemon) in group.daemons.iter_mut().enumerate() {
                // First, check if there's a pending restart that's ready
                if let Some(restart_at) = group.pending_restarts[idx] {
                    if now >= restart_at {
                        tracing::info!(daemon = %daemon.name(), "restarting daemon");
                        daemon.start().map_err(DaemonError::Process)?;

                        if let Some(reader) = daemon.try_clone_reader() {
                            pty_readers.push((
                                daemon.name().to_string(),
                                Some(group_name.clone()),
                                reader,
                            ));
                        }

                        group.state.daemons[idx].status = ProcessStatus::Running;
                        group.state.daemons[idx].pid = daemon.pid();
                        group.pending_restarts[idx] = None;
                    }

                    // Skip any daemons whose time hasn't come
                    continue;
                }

                // If daemon exited, update the state and schedule restart
                let exit_info = daemon.check().await.map_err(DaemonError::Process)?;
                if let Some(exit_info) = exit_info {
                    group.state.daemons[idx].status = ProcessStatus::Backoff;
                    group.state.daemons[idx].pid = None;

                    let delay = daemon.restart_delay();
                    tracing::info!(
                        daemon = %daemon.name(),
                        delay_ms = delay.as_millis(),
                        "daemon exited, scheduling restart"
                    );

                    // Log the daemon exit with duration and exit code
                    let exit_code_str = exit_info
                        .exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let log_msg = format!(
                        "exited after {:.2}s (exit code: {})",
                        exit_info.duration.as_secs_f64(),
                        exit_code_str
                    );
                    let _ = self
                        .log_store
                        .sender()
                        .send(
                            LogLine::daemon(daemon.name(), log_msg).with_group(group_name.clone()),
                        )
                        .await;

                    group.pending_restarts[idx] = Some(now + delay);
                }
            }
        }

        for (process, group, reader) in pty_readers {
            self.spawn_pty_reader(process, group, reader);
        }

        self.update_state();
        Ok(())
    }

    /// Shutdown all processes gracefully.
    ///
    /// Sends SIGTERM to all daemons, waits up to grace_period for them to exit,
    /// then sends SIGKILL to any that are still running.
    pub async fn shutdown(&mut self) -> Result<(), DaemonError> {
        const GRACE_PERIOD: Duration = Duration::from_secs(10);
        const POLL_INTERVAL: Duration = Duration::from_millis(100);

        tracing::info!("shutting down");
        self.state.status = DaemonStatus::Stopping;

        // Send SIGTERM to all daemons
        for group in self.groups.values_mut() {
            for daemon in &mut group.daemons {
                daemon.stop().map_err(DaemonError::Process)?;
            }
        }

        // Wait for daemons to exit, up to grace period
        let deadline = std::time::Instant::now() + GRACE_PERIOD;
        loop {
            let mut any_running = false;
            for group in self.groups.values_mut() {
                for daemon in &mut group.daemons {
                    if daemon.is_running() {
                        any_running = true;
                    }
                }
            }

            if !any_running {
                tracing::info!("all daemons exited");
                break;
            }

            if std::time::Instant::now() >= deadline {
                tracing::warn!("grace period expired, force killing remaining daemons");
                for group in self.groups.values_mut() {
                    for daemon in &mut group.daemons {
                        if daemon.is_running() {
                            daemon.kill().map_err(DaemonError::Process)?;
                        }
                    }
                }
                break;
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }

        Ok(())
    }

    /// Restart a specific group.
    ///
    /// Spawns the group execution asynchronously. Returns immediately.
    pub fn restart_group(&mut self, group_name: &str) -> Result<(), DaemonError> {
        if !self.groups.contains_key(group_name) {
            return Err(DaemonError::GroupNotFound(group_name.to_string()));
        }
        tracing::info!(group = group_name, "restarting group");
        let trigger_ctx =
            self.manual_restart_context(RestartScope::Group(group_name.to_string()));
        self.spawn_group_tasks(group_name, &trigger_ctx);
        Ok(())
    }

    /// Restart all groups.
    ///
    /// Spawns all group executions asynchronously. Returns immediately.
    pub fn restart_all(&mut self) -> Result<(), DaemonError> {
        tracing::info!("restarting all groups");
        let trigger_ctx = self.manual_restart_context(RestartScope::All);
        for group_name in &self.execution_order.clone() {
            self.spawn_group_tasks(group_name, &trigger_ctx);
        }
        Ok(())
    }

    /// Reload configuration from the file.
    ///
    /// This:
    /// 1. Parses and validates the new config
    /// 2. Stops daemons in removed/modified groups
    /// 3. Updates group configurations
    /// 4. Starts daemons in new/modified groups
    /// 5. Broadcasts reload status
    pub async fn reload_config(&mut self) -> ReloadResult {
        tracing::info!(path = %self.config_path.display(), "reloading configuration");

        // 1. Parse and validate new config
        let new_config = match zaz_config::load(&self.config_path) {
            Ok(config) => config,
            Err(e) => {
                tracing::error!(error = %e, "failed to parse config");
                return ReloadResult::Failed(format!("parse error: {}", e));
            }
        };

        // 2. Compute changes
        let (added, removed, modified) = self.get_config_changes(&new_config);

        // 3. Stop daemons in removed groups
        for group_name in &removed {
            if let Some(group) = self.groups.get_mut(group_name) {
                for daemon in &mut group.daemons {
                    if let Err(e) = daemon.stop() {
                        tracing::warn!(
                            daemon = %daemon.name(),
                            group = %group_name,
                            error = %e,
                            "failed to stop daemon during reload"
                        );
                    }
                }
            }
        }

        // 4. Stop daemons in modified groups (they'll be restarted)
        for group_name in &modified {
            if let Some(group) = self.groups.get_mut(group_name) {
                for daemon in &mut group.daemons {
                    if let Err(e) = daemon.stop() {
                        tracing::warn!(
                            daemon = %daemon.name(),
                            group = %group_name,
                            error = %e,
                            "failed to stop daemon during reload"
                        );
                    }
                }
            }
        }

        // 5. Compute new execution order
        let execution_order = match topological_sort(&new_config.groups) {
            Ok(order) => order,
            Err(e) => {
                tracing::error!(error = %e, "failed to compute execution order");
                return ReloadResult::Failed(format!("dependency error: {}", e));
            }
        };

        // 6. Apply new configuration
        self.config = new_config;
        self.execution_order = execution_order;

        // 6b. Rebuild dependency resolver from new config
        self.dependency_resolver = DependencyResolver::from_groups(
            self.config
                .groups
                .iter()
                .map(|g| (g.name.as_str(), g.depends_on.as_slice())),
        );

        // 7. Rebuild groups
        if let Err(e) = self.rebuild_groups() {
            tracing::error!(error = %e, "failed to rebuild groups");
            return ReloadResult::Failed(format!("rebuild error: {}", e));
        }

        // 8. Run new/modified groups
        for group_name in added.iter().chain(&modified) {
            if let Err(e) = self.run_group(group_name, &[], false).await {
                tracing::error!(group = %group_name, error = %e, "failed to start group after reload");
            }
        }

        // 9. Broadcast reload complete
        self.push_log(LogLine::daemon(
            "zaz",
            format!(
                "configuration reloaded: {} added, {} removed, {} modified",
                added.len(),
                removed.len(),
                modified.len()
            ),
        ));
        self.update_state();

        ReloadResult::Success {
            added,
            removed,
            modified,
        }
    }

    /// Compute changes between current config and new config.
    fn get_config_changes(
        &self,
        new_config: &Config,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        compute_config_changes(&self.config.groups, &new_config.groups)
    }

    /// Rebuild group patterns and managed groups from current config.
    fn rebuild_groups(&mut self) -> Result<(), DaemonError> {
        // Clear and rebuild group patterns
        self.group_patterns.clear();
        let mut new_groups = IndexMap::new();

        // Get config directory for default working directory
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));

        for group in &self.config.groups {
            // Create pattern set for this group
            let patterns =
                PatternSet::new(&group.patterns, &group.ignore).map_err(DaemonError::Watch)?;
            self.group_patterns.insert(group.name.clone(), patterns);

            // Create executor with shell, working directory, and group env
            // Default to config directory if no explicit working_dir is set
            let mut executor = Executor::new(self.config.settings.shell.clone());
            let working_dir = group
                .working_dir
                .clone()
                .unwrap_or_else(|| config_dir.to_string_lossy().to_string());
            executor = executor.with_working_dir(working_dir);
            if !group.env.is_empty() {
                executor = executor.with_env(group.env.clone());
            }

            // Create daemons with per-daemon working_dir and env overrides
            let daemons: Vec<Daemon> = group
                .daemons
                .iter()
                .map(|d| {
                    let mut daemon_executor = executor.clone();
                    if let Some(ref dir) = d.working_dir {
                        daemon_executor = daemon_executor.with_working_dir(dir.clone());
                    }
                    if !d.env.is_empty() {
                        daemon_executor = daemon_executor.extend_env(d.env.clone());
                    }
                    Daemon::new(d.clone(), daemon_executor)
                })
                .collect();

            // Initialize group state
            let state = GroupState {
                name: group.name.clone(),
                status: GroupStatus::Pending,
                tasks: group
                    .tasks
                    .iter()
                    .map(|t| ProcessState {
                        name: t.name().to_string(),
                        status: ProcessStatus::Pending,
                        ..Default::default()
                    })
                    .collect(),
                daemons: group
                    .daemons
                    .iter()
                    .map(|d| ProcessState {
                        name: d.name().to_string(),
                        status: ProcessStatus::Pending,
                        ..Default::default()
                    })
                    .collect(),
            };

            let daemon_count = daemons.len();
            new_groups.insert(
                group.name.clone(),
                ManagedGroup {
                    config: group.clone(),
                    executor,
                    daemons,
                    state,
                    daemons_started: false,
                    pending_restarts: vec![None; daemon_count],
                },
            );
        }

        self.groups = new_groups;

        // Update daemon state
        self.state.groups = self
            .groups
            .iter()
            .map(|(k, v)| (k.clone(), v.state.clone()))
            .collect();

        Ok(())
    }

    /// Restart a specific process (task or daemon) within a group.
    pub async fn restart_process(
        &mut self,
        group_name: &str,
        process_name: &str,
    ) -> Result<(), DaemonError> {
        // Check if group exists
        if !self.groups.contains_key(group_name) {
            return Err(DaemonError::GroupNotFound(group_name.to_string()));
        }

        // Build trigger context for process restart
        let trigger_ctx = self.manual_restart_context(RestartScope::Process {
            group: group_name.to_string(),
            process: process_name.to_string(),
        });

        // Check if it's a task
        let group = self.groups.get(group_name).unwrap();
        if let Some((task_idx, task_config)) = group
            .config
            .tasks
            .iter()
            .enumerate()
            .find(|(_, t)| t.name() == process_name)
        {
            tracing::info!(
                group = group_name,
                task = process_name,
                trigger_source = ?trigger_ctx.source,
                "restarting task"
            );

            let expander = zaz_vars::Expander::new(&trigger_ctx.vars);
            let command = expander
                .expand(&task_config.command)
                .map_err(|e| DaemonError::VarExpansion(e.to_string()))?;

            // Use task-specific working_dir and env if set
            let mut executor = group.executor.clone();
            if let Some(ref dir) = task_config.working_dir {
                executor = executor.with_working_dir(dir.clone());
            }
            if !task_config.env.is_empty() {
                executor = executor.extend_env(task_config.env.clone());
            }
            let silence = task_config.silence;

            // Spawn task in background (same as restart_group) for proper log streaming
            self.spawn_task(
                group_name,
                process_name,
                task_idx,
                command,
                executor,
                silence,
            );

            return Ok(());
        }

        // Check if it's a daemon
        let group = self.groups.get(group_name).unwrap();
        if let Some((daemon_idx, _)) = group
            .daemons
            .iter()
            .enumerate()
            .find(|(_, d)| d.name() == process_name)
        {
            tracing::info!(
                group = group_name,
                daemon = process_name,
                trigger_source = ?trigger_ctx.source,
                should_cascade = trigger_ctx.should_cascade,
                "restarting daemon"
            );

            self.push_log(
                LogLine::daemon(process_name, "restarting").with_group(group_name.to_string()),
            );

            if let Some(group) = self.groups.get_mut(group_name) {
                group.daemons[daemon_idx]
                    .signal_restart()
                    .map_err(DaemonError::Process)?;
            }

            // Cascade restart to dependent groups if enabled
            if trigger_ctx.should_cascade {
                if let Err(e) = self.cascade_daemon_restart(group_name).await {
                    tracing::error!(
                        group = %group_name,
                        error = %e,
                        "failed to cascade daemon restart"
                    );
                }
            }

            self.update_state();
            return Ok(());
        }

        Err(DaemonError::TaskFailed {
            task: process_name.to_string(),
            error: format!(
                "process '{}' not found in group '{}'",
                process_name, group_name
            ),
        })
    }

    /// Handle an API request and return a response.
    ///
    /// For Subscribe/SubscribeLogs requests, returns Ok to acknowledge,
    /// then the caller should use `subscribe()` to get the broadcast receiver.
    pub async fn handle_request(&mut self, request: crate::ApiRequest) -> crate::ApiResponse {
        use crate::{ApiRequest, ApiResponse};

        match request {
            ApiRequest::Status | ApiRequest::ListGroups => {
                self.update_state();
                ApiResponse::Status {
                    state: self.state.clone(),
                }
            }
            ApiRequest::Subscribe => {
                // Caller should use engine.subscribe() to get broadcast receiver
                self.update_state();
                ApiResponse::Status {
                    state: self.state.clone(),
                }
            }
            ApiRequest::GetLogs {
                name,
                lines,
                offset,
                limit,
                search,
            } => {
                // Use new pagination if any pagination param is set, otherwise use legacy
                let use_pagination = offset.is_some() || limit.is_some() || search.is_some();

                if use_pagination {
                    // Build query from request parameters
                    let mut query = if name == "*" {
                        LogQuery::all()
                    } else {
                        LogQuery::process(&name)
                    };

                    if let Some(off) = offset {
                        query = query.with_offset(off);
                    }
                    if let Some(lim) = limit {
                        query = query.with_limit(lim);
                    } else if let Some(lines_limit) = lines {
                        // Fall back to legacy `lines` parameter
                        query = query.with_limit(lines_limit);
                    }
                    if let Some(ref pattern) = search {
                        query = query.with_search(pattern);
                    }

                    let result = self.query_logs(query);
                    ApiResponse::Logs {
                        name,
                        lines: result.logs,
                        total_count: Some(result.total_count),
                        has_more: Some(result.has_more),
                        offset: Some(result.offset),
                    }
                } else {
                    // Legacy behavior: just return logs with optional limit
                    let logs = self.get_logs(&name, lines);
                    ApiResponse::Logs {
                        name,
                        lines: logs,
                        total_count: None,
                        has_more: None,
                        offset: None,
                    }
                }
            }
            ApiRequest::SubscribeLogs { name } => {
                // Return current logs; caller should use subscribe_logs() for streaming
                let logs = self.get_logs(&name, Some(100));
                ApiResponse::Logs {
                    name,
                    lines: logs,
                    total_count: None,
                    has_more: None,
                    offset: None,
                }
            }
            ApiRequest::RestartGroup { name } => match self.restart_group(&name) {
                Ok(()) => {
                    ApiResponse::ok_with_message(format!("restart initiated for group '{}'", name))
                }
                Err(e) => ApiResponse::error(format!("failed to restart group '{}': {}", name, e)),
            },
            ApiRequest::RestartProcess { group, process } => {
                match self.restart_process(&group, &process).await {
                    Ok(()) => ApiResponse::ok_with_message(format!("restarted '{}'", process)),
                    Err(e) => ApiResponse::error(format!(
                        "failed to restart '{}' in group '{}': {}",
                        process, group, e
                    )),
                }
            }
            ApiRequest::RestartAll => match self.restart_all() {
                Ok(()) => ApiResponse::ok_with_message("restart initiated for all groups"),
                Err(e) => ApiResponse::error(format!("failed to restart: {}", e)),
            },
            ApiRequest::ReloadConfig => match self.reload_config().await {
                ReloadResult::Success {
                    added,
                    removed,
                    modified,
                } => ApiResponse::ok_with_message(format!(
                    "config reloaded: {} added, {} removed, {} modified",
                    added.len(),
                    removed.len(),
                    modified.len()
                )),
                ReloadResult::Failed(e) => ApiResponse::error(format!("reload failed: {}", e)),
            },
            ApiRequest::Shutdown => {
                if self.embedded {
                    ApiResponse::error(
                        "cannot stop embedded daemon; use the TUI to quit or press Ctrl+C",
                    )
                } else {
                    // Signal handled by caller
                    ApiResponse::ok_with_message("shutting down")
                }
            }
        }
    }

    /// Create a combined pattern set from all groups.
    fn combined_patterns(&self) -> Result<PatternSet, DaemonError> {
        let mut includes = Vec::new();
        let mut ignores = Vec::new();

        for group in &self.config.groups {
            includes.extend(group.patterns.clone());
            ignores.extend(group.ignore.clone());
        }

        PatternSet::new(&includes, &ignores).map_err(DaemonError::Watch)
    }

    // =========================================================================
    // Dependency helper methods
    // =========================================================================

    /// Get the dependencies for a group.
    fn get_group_dependencies(&self, group_name: &str) -> Vec<String> {
        self.dependency_resolver.get_dependencies(group_name)
    }

    /// Set a group's status (keeps Engine and DependencyResolver in sync).
    ///
    /// Note: This only sets the status. For Ready/Failed status changes that need
    /// to trigger dependents or cascade skips, use the higher-level methods like
    /// `trigger_dependents()` or `cascade_skip()`.
    fn set_group_status(&mut self, group_name: &str, status: GroupStatus) {
        if let Some(group) = self.groups.get_mut(group_name) {
            group.state.status = status;
        }
        self.dependency_resolver.set_status(group_name, status);
    }

    // =========================================================================
    // TriggerContext Factory Methods
    // =========================================================================

    /// Create a trigger context for initial startup.
    fn startup_context(&self) -> TriggerContext {
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        TriggerContext {
            source: TriggerSource::Startup,
            vars: Context::new()
                .with_variables(self.config.variables.clone())
                .with_root(config_dir.to_path_buf()),
            run_on_change_tasks: false, // Don't run on_change_only tasks during startup
            should_cascade: true,
        }
    }

    /// Create a trigger context for file change events.
    fn file_change_context(&self, files: Vec<PathBuf>) -> TriggerContext {
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        TriggerContext {
            source: TriggerSource::FileChange {
                files: files.clone(),
            },
            vars: Context::new()
                .with_variables(self.config.variables.clone())
                .with_files(files)
                .with_root(config_dir.to_path_buf()),
            run_on_change_tasks: true, // Run all tasks including on_change_only
            should_cascade: true,
        }
    }

    /// Create a trigger context for manual restart requests.
    fn manual_restart_context(&self, scope: RestartScope) -> TriggerContext {
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        TriggerContext {
            source: TriggerSource::ManualRestart { scope },
            vars: Context::new()
                .with_variables(self.config.variables.clone())
                .with_root(config_dir.to_path_buf()),
            run_on_change_tasks: false, // Don't run on_change_only tasks for manual restarts
            should_cascade: true,        // Manual restarts cascade to dependents
        }
    }

    /// Create a trigger context for when a dependency becomes ready.
    fn dependency_ready_context(&self, completed_group: &str) -> TriggerContext {
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        TriggerContext {
            source: TriggerSource::DependencyReady {
                completed_group: completed_group.to_string(),
            },
            vars: Context::new()
                .with_variables(self.config.variables.clone())
                .with_root(config_dir.to_path_buf()),
            run_on_change_tasks: false, // Don't run on_change_only for dependency triggers
            should_cascade: true,
        }
    }

    /// Create a trigger context for daemon restart cascades.
    fn daemon_restart_context(&self, source_group: &str) -> TriggerContext {
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        TriggerContext {
            source: TriggerSource::DaemonRestart {
                group: source_group.to_string(),
            },
            vars: Context::new()
                .with_variables(self.config.variables.clone())
                .with_root(config_dir.to_path_buf()),
            run_on_change_tasks: false, // Re-run startup tasks only
            should_cascade: true,
        }
    }

    /// Create a trigger context for task completion events.
    fn task_completion_context(&self, completed_group: &str) -> TriggerContext {
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        TriggerContext {
            source: TriggerSource::TaskCompletion {
                group: completed_group.to_string(),
            },
            vars: Context::new()
                .with_variables(self.config.variables.clone())
                .with_root(config_dir.to_path_buf()),
            run_on_change_tasks: false,
            should_cascade: true,
        }
    }

    /// Determine the lifecycle phase for a group.
    ///
    /// A group is in Runtime phase if it has already completed initial startup:
    /// - For groups with daemons: when daemons have been started at least once
    /// - For groups without daemons: when no dependents are waiting for it
    ///
    /// The key insight is that during startup, dependent groups are in "Waiting"
    /// state tracked by the DependencyResolver. During runtime, dependents are
    /// already in "Ready" state and won't be in the waiting set.
    fn determine_lifecycle_phase(&self, group_name: &str) -> LifecyclePhase {
        self.groups
            .get(group_name)
            .map(|g| {
                // Check if group has daemons
                let has_daemons = !g.daemons.is_empty();

                if has_daemons {
                    // Groups with daemons: runtime after first start
                    if g.daemons_started {
                        LifecyclePhase::Runtime
                    } else {
                        LifecyclePhase::Startup
                    }
                } else {
                    // Groups without daemons: check if any dependents are waiting
                    // If dependents are waiting, we're in startup. If not, runtime.
                    if self.dependency_resolver.has_waiting_dependents(group_name) {
                        LifecyclePhase::Startup
                    } else {
                        LifecyclePhase::Runtime
                    }
                }
            })
            .unwrap_or(LifecyclePhase::Startup)
    }

    /// Trigger dependents of a completed group.
    ///
    /// Called when a group successfully completes (reaches Ready status).
    /// This checks all groups that depend on the completed group and starts
    /// them if all their dependencies are now satisfied.
    fn trigger_dependents(&mut self, completed_group: &str) {
        // Use the resolver to mark the group complete and get resulting actions
        let result = self.dependency_resolver.mark_complete(completed_group);

        // Sync Engine's status for the completed group
        if let Some(group) = self.groups.get_mut(completed_group) {
            group.state.status = GroupStatus::Ready;
        }

        let trigger_ctx = self.dependency_ready_context(completed_group);

        // Start groups that are now ready
        for group_name in result.ready_to_start {
            tracing::info!(
                group = %group_name,
                triggered_by = %completed_group,
                "starting group after dependency completed"
            );
            self.start_waiting_group(&group_name, &trigger_ctx);
        }

        // Cascade skip for groups that have a failed dependency
        for group_name in result.needs_skip {
            tracing::info!(
                group = %group_name,
                "skipping group due to failed dependency"
            );
            self.cascade_skip(&group_name);
        }
    }

    /// Start a group that was waiting for dependencies.
    fn start_waiting_group(&mut self, group_name: &str, trigger_ctx: &TriggerContext) {
        let has_startup_tasks = self
            .groups
            .get(group_name)
            .map(|g| g.config.tasks.iter().any(|t| !t.on_change_only))
            .unwrap_or(false);

        if has_startup_tasks {
            self.spawn_group_tasks(group_name, trigger_ctx);
        } else {
            // No tasks - mark as ready immediately or start daemons
            let has_daemons = self
                .groups
                .get(group_name)
                .map(|g| !g.daemons.is_empty())
                .unwrap_or(false);

            if has_daemons {
                // Queue daemon start - this will be handled asynchronously
                // We need to spawn a task to handle this since we can't await here
                let group_name = group_name.to_string();
                let task_completion_tx = self.task_completion_tx.clone();
                tokio::spawn(async move {
                    // Signal that this group needs daemon startup
                    // We use a special task completion to trigger daemon startup
                    let _ = task_completion_tx
                        .send(TaskCompletion {
                            group_name,
                            task_id: "__daemon_start__".to_string(),
                            task_index: 0,
                            success: true,
                            status: ProcessStatus::Success,
                            duration_ms: None,
                            exit_code: None,
                        })
                        .await;
                });
            } else {
                // No tasks and no daemons - mark as Ready and trigger dependents
                self.set_group_status(group_name, GroupStatus::Ready);
                self.trigger_dependents(group_name);
            }
        }
    }

    /// Cascade skip status to dependents when a group fails.
    ///
    /// Marks the group as Skipped and recursively skips all dependents
    /// that were waiting for it.
    fn cascade_skip(&mut self, group_name: &str) {
        // Use the resolver to mark as skipped and get cascading skips
        let result = self.dependency_resolver.mark_skipped(group_name);

        // Sync Engine's status for the source group
        if let Some(group) = self.groups.get_mut(group_name) {
            group.state.status = GroupStatus::Skipped;
        }

        // Sync Engine's status for all transitively skipped groups
        for skipped_group in result.to_skip {
            if let Some(group) = self.groups.get_mut(&skipped_group) {
                group.state.status = GroupStatus::Skipped;
            }
        }
    }

    /// Propagate completion to dependent groups based on lifecycle phase.
    ///
    /// This is the unified entry point for cascade logic. The behavior differs
    /// based on the lifecycle phase:
    ///
    /// - **Startup phase**: Groups are completing for the first time. Uses
    ///   `trigger_dependents` to start groups that were waiting for dependencies.
    ///
    /// - **Runtime phase**: Groups are already Ready and being re-triggered (e.g.,
    ///   due to file changes or daemon restarts). Uses `cascade_daemon_restart`
    ///   to propagate restarts to dependent groups.
    async fn propagate_to_dependents(
        &mut self,
        completed_group: &str,
        phase: LifecyclePhase,
    ) -> Result<(), DaemonError> {
        match phase {
            LifecyclePhase::Startup => {
                self.trigger_dependents(completed_group);
                Ok(())
            }
            LifecyclePhase::Runtime => self.cascade_daemon_restart(completed_group).await,
        }
    }

    /// Cascade restart to dependent groups.
    ///
    /// When a group is restarted, this propagates the restart to all dependent
    /// groups (transitively). For each dependent:
    /// - If it has startup tasks: spawn those tasks (cascade continues when they complete)
    /// - If it has no tasks but has daemons: signal daemons directly and continue cascade
    async fn cascade_daemon_restart(&mut self, source_group: &str) -> Result<(), DaemonError> {
        let dependents = self.dependency_resolver.get_dependents(source_group);
        if dependents.is_empty() {
            return Ok(());
        }

        let trigger_ctx = self.daemon_restart_context(source_group);

        for dependent in dependents {
            // Get dependent group info
            let (is_ready, has_startup_tasks, has_running_daemons) = self
                .groups
                .get(&dependent)
                .map(|g| {
                    let is_ready = g.state.status == GroupStatus::Ready;
                    let has_startup_tasks = g.config.tasks.iter().any(|t| !t.on_change_only);
                    let has_running_daemons = g.daemons_started && !g.daemons.is_empty();
                    (is_ready, has_startup_tasks, has_running_daemons)
                })
                .unwrap_or((false, false, false));

            // Only cascade if the dependent is Ready (completed initial startup)
            // and has something to trigger (tasks to run or daemons to restart)
            let should_cascade = is_ready && (has_startup_tasks || has_running_daemons);

            if should_cascade {
                if has_startup_tasks {
                    tracing::info!(
                        from = %source_group,
                        to = %dependent,
                        "cascading restart: spawning tasks for dependent group"
                    );

                    // Spawn tasks - when they complete, daemons will be signaled
                    // and cascade will continue through process_task_completions
                    self.spawn_group_tasks(&dependent, &trigger_ctx);
                } else {
                    tracing::info!(
                        from = %source_group,
                        to = %dependent,
                        "cascading restart: signaling daemons directly (no tasks)"
                    );

                    // No tasks - signal daemons directly (without cascade, we handle it below)
                    if let Err(e) = self.signal_group_daemons_no_cascade(&dependent) {
                        tracing::error!(
                            group = %dependent,
                            error = %e,
                            "failed to cascade daemon restart"
                        );
                    }

                    // Recursively cascade to further dependents
                    Box::pin(self.cascade_daemon_restart(&dependent)).await?;
                }
            }
        }

        Ok(())
    }

    /// Signal all daemons in a group to restart (without cascading).
    ///
    /// This is a low-level method used internally by cascade_daemon_restart.
    /// For most cases, use `restart_group_daemons` which also triggers the cascade.
    fn signal_group_daemons_no_cascade(&mut self, group_name: &str) -> Result<(), DaemonError> {
        if let Some(group) = self.groups.get_mut(group_name) {
            for daemon in &mut group.daemons {
                if daemon.is_running() {
                    daemon.signal_restart().map_err(DaemonError::Process)?;
                }
            }
        }
        Ok(())
    }

    /// Update the internal state from group states and broadcast to subscribers.
    fn update_state(&mut self) {
        self.state.groups = self
            .groups
            .iter()
            .map(|(k, v)| (k.clone(), v.state.clone()))
            .collect();

        // Broadcast status update to subscribers (ignore send errors - no subscribers)
        let _ = self.status_tx.send(ApiResponse::StatusUpdate {
            state: self.state.clone(),
        });
    }
}

/// Filter affected groups to only root groups (groups with no affected dependencies).
///
/// When multiple groups are affected by file changes, we only want to trigger the "root"
/// groups - those whose dependencies are NOT also affected. Dependents will be triggered
/// through the cascade mechanism when their dependencies complete.
fn filter_affected_to_roots<F>(affected_groups: &[String], get_dependencies: F) -> Vec<String>
where
    F: Fn(&str) -> Vec<String>,
{
    use std::collections::HashSet;

    let affected_set: HashSet<&String> = affected_groups.iter().collect();
    affected_groups
        .iter()
        .filter(|g| {
            let deps = get_dependencies(g);
            // Keep this group only if none of its dependencies are also affected
            !deps.iter().any(|dep| affected_set.contains(dep))
        })
        .cloned()
        .collect()
}

/// Calculate group status from task states.
///
/// Returns `None` if there are still tasks running, otherwise returns the final status.
fn calculate_group_status_from_tasks(
    task_states: &[ProcessState],
    has_running_tasks: bool,
) -> Option<GroupStatus> {
    if has_running_tasks {
        return None;
    }

    let any_failed = task_states
        .iter()
        .any(|t| t.status == ProcessStatus::Failed);

    Some(if any_failed {
        GroupStatus::Failed
    } else {
        GroupStatus::Ready
    })
}

/// Compute changes between old and new group configurations.
///
/// Returns (added, removed, modified) group names.
fn compute_config_changes(
    old_groups: &[Group],
    new_groups: &[Group],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    use std::collections::HashSet;

    let old_names: HashSet<_> = old_groups.iter().map(|g| &g.name).collect();
    let new_names: HashSet<_> = new_groups.iter().map(|g| &g.name).collect();

    let added: Vec<String> = new_names
        .difference(&old_names)
        .map(|s| (*s).clone())
        .collect();
    let removed: Vec<String> = old_names
        .difference(&new_names)
        .map(|s| (*s).clone())
        .collect();

    // Check for modifications (compare serialized versions)
    let mut modified = Vec::new();
    for new_group in new_groups {
        if let Some(old_group) = old_groups.iter().find(|g| g.name == new_group.name) {
            let old_json = serde_json::to_string(old_group).unwrap_or_default();
            let new_json = serde_json::to_string(new_group).unwrap_or_default();
            if old_json != new_json {
                modified.push(new_group.name.clone());
            }
        }
    }

    (added, removed, modified)
}

/// Topologically sort groups based on dependencies.
fn topological_sort(groups: &[Group]) -> Result<Vec<String>, DaemonError> {
    let mut result = Vec::new();
    let mut visited = HashMap::new();
    let mut temp_mark = HashMap::new();

    let group_map: HashMap<&str, &Group> = groups.iter().map(|g| (g.name.as_str(), g)).collect();

    fn visit<'a>(
        name: &'a str,
        group_map: &HashMap<&str, &'a Group>,
        visited: &mut HashMap<&'a str, bool>,
        temp_mark: &mut HashMap<&'a str, bool>,
        result: &mut Vec<String>,
    ) -> Result<(), DaemonError> {
        if visited.get(name).copied().unwrap_or(false) {
            return Ok(());
        }
        if temp_mark.get(name).copied().unwrap_or(false) {
            return Err(DaemonError::CyclicDependency(name.to_string()));
        }

        temp_mark.insert(name, true);

        if let Some(group) = group_map.get(name) {
            for dep in &group.depends_on {
                visit(dep, group_map, visited, temp_mark, result)?;
            }
        }

        temp_mark.insert(name, false);
        visited.insert(name, true);
        result.push(name.to_string());

        Ok(())
    }

    for group in groups {
        visit(
            &group.name,
            &group_map,
            &mut visited,
            &mut temp_mark,
            &mut result,
        )?;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiRequest;
    use zaz_config::{Group, Silence, TaskCommand};

    // =========================================================================
    // Test helpers
    // =========================================================================

    /// Create a minimal Engine for testing.
    ///
    /// Uses a temp directory for file watching to avoid needing real files.
    fn create_test_engine(groups: Vec<Group>) -> Engine {
        use std::fs;

        // Create temp directory for watcher
        let temp_dir = std::env::temp_dir().join("zaz-test");
        fs::create_dir_all(&temp_dir).unwrap();

        let config = Config {
            settings: zaz_config::Settings::default(),
            variables: HashMap::new(),
            groups,
        };

        // Create a fake config path in temp dir
        let config_path = temp_dir.join("zaz.yaml");

        Engine::from_config(config, config_path, false, false).unwrap()
    }

    /// Create a test group with specified tasks.
    fn test_group(name: &str, task_names: &[&str]) -> Group {
        Group {
            name: name.to_string(),
            tasks: task_names
                .iter()
                .map(|n| TaskCommand::new(*n, "echo test"))
                .collect(),
            patterns: vec!["*.test".to_string()],
            ..Default::default()
        }
    }

    /// Create a test group with dependencies.
    fn test_group_with_deps(name: &str, task_names: &[&str], depends_on: &[&str]) -> Group {
        let mut group = test_group(name, task_names);
        group.depends_on = depends_on.iter().map(|s| s.to_string()).collect();
        group
    }

    /// Create a TaskCompletion for testing.
    fn task_completion(group: &str, task: &str, task_index: usize, success: bool) -> TaskCompletion {
        TaskCompletion {
            task_id: format!("{}:{}", group, task),
            group_name: group.to_string(),
            task_index,
            success,
            status: if success {
                ProcessStatus::Success
            } else {
                ProcessStatus::Failed
            },
            duration_ms: Some(100),
            exit_code: Some(if success { 0 } else { 1 }),
        }
    }

    // =========================================================================
    // process_task_completions() state machine tests
    // =========================================================================

    #[tokio::test]
    async fn test_process_completion_updates_task_state() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        // Simulate task running
        engine.running_tasks.insert("mygroup:task1".to_string());
        engine.groups.get_mut("mygroup").unwrap().state.tasks[0].status = ProcessStatus::Running;

        // Send completion
        let completion = task_completion("mygroup", "task1", 0, true);
        engine.task_completion_tx.send(completion).await.unwrap();

        // Process completions
        engine.process_task_completions().await;

        // Verify task state updated
        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.tasks[0].status, ProcessStatus::Success);
        assert_eq!(group.state.tasks[0].duration_ms, Some(100));
        assert_eq!(group.state.tasks[0].exit_code, Some(0));

        // Running tasks should be empty
        assert!(engine.running_tasks.is_empty());
    }

    #[tokio::test]
    async fn test_process_completion_failed_task_updates_state() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        // Simulate task running
        engine.running_tasks.insert("mygroup:task1".to_string());
        engine.groups.get_mut("mygroup").unwrap().state.tasks[0].status = ProcessStatus::Running;

        // Send failed completion
        let completion = task_completion("mygroup", "task1", 0, false);
        engine.task_completion_tx.send(completion).await.unwrap();

        // Process completions
        engine.process_task_completions().await;

        // Verify task state updated
        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.tasks[0].status, ProcessStatus::Failed);
        assert_eq!(group.state.tasks[0].exit_code, Some(1));
    }

    #[tokio::test]
    async fn test_handle_api_get_logs_legacy_response_omits_pagination_metadata() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        engine.push_log(LogLine::process("task1", "line 1").with_group("mygroup"));
        engine.push_log(LogLine::process("task1", "line 2").with_group("mygroup"));

        let response = engine
            .handle_request(ApiRequest::GetLogs {
                name: "task1".to_string(),
                lines: Some(1),
                offset: None,
                limit: None,
                search: None,
            })
            .await;

        match response {
            ApiResponse::Logs {
                name,
                lines,
                total_count,
                has_more,
                offset,
            } => {
                assert_eq!(name, "task1");
                assert_eq!(lines.len(), 1);
                assert_eq!(lines[0].content, "line 2");
                assert_eq!(total_count, None);
                assert_eq!(has_more, None);
                assert_eq!(offset, None);
            }
            other => panic!("expected logs response, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_handle_api_get_logs_pagination_returns_metadata() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        for i in 0..5 {
            engine.push_log(
                LogLine::process("task1", format!("error line {}", i)).with_group("mygroup"),
            );
        }

        let response = engine
            .handle_request(ApiRequest::GetLogs {
                name: "task1".to_string(),
                lines: None,
                offset: Some(1),
                limit: Some(2),
                search: Some("ERROR".to_string()),
            })
            .await;

        match response {
            ApiResponse::Logs {
                name,
                lines,
                total_count,
                has_more,
                offset,
            } => {
                assert_eq!(name, "task1");
                assert_eq!(lines.len(), 2);
                assert_eq!(lines[0].content, "error line 1");
                assert_eq!(lines[1].content, "error line 2");
                assert_eq!(total_count, Some(5));
                assert_eq!(has_more, Some(true));
                assert_eq!(offset, Some(1));
            }
            other => panic!("expected logs response, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_process_completion_group_ready_when_all_tasks_succeed() {
        let groups = vec![test_group("mygroup", &["task1", "task2"])];
        let mut engine = create_test_engine(groups);

        // Set group to running
        engine.groups.get_mut("mygroup").unwrap().state.status = GroupStatus::Running;

        // Simulate both tasks running
        engine.running_tasks.insert("mygroup:task1".to_string());
        engine.running_tasks.insert("mygroup:task2".to_string());
        engine.groups.get_mut("mygroup").unwrap().state.tasks[0].status = ProcessStatus::Running;
        engine.groups.get_mut("mygroup").unwrap().state.tasks[1].status = ProcessStatus::Running;

        // Complete task1 - group should stay Running
        let completion1 = task_completion("mygroup", "task1", 0, true);
        engine.task_completion_tx.send(completion1).await.unwrap();
        engine.process_task_completions().await;

        // Group still running (task2 still running)
        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.status, GroupStatus::Running);

        // Complete task2 - group should become Ready
        let completion2 = task_completion("mygroup", "task2", 1, true);
        engine.task_completion_tx.send(completion2).await.unwrap();
        engine.process_task_completions().await;

        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.status, GroupStatus::Ready);
    }

    #[tokio::test]
    async fn test_process_completion_group_failed_when_any_task_fails() {
        let groups = vec![test_group("mygroup", &["task1", "task2"])];
        let mut engine = create_test_engine(groups);

        // Set group to running
        engine.groups.get_mut("mygroup").unwrap().state.status = GroupStatus::Running;

        // Simulate both tasks running
        engine.running_tasks.insert("mygroup:task1".to_string());
        engine.running_tasks.insert("mygroup:task2".to_string());

        // task1 succeeds
        let completion1 = task_completion("mygroup", "task1", 0, true);
        engine.task_completion_tx.send(completion1).await.unwrap();
        engine.process_task_completions().await;

        // task2 fails
        let completion2 = task_completion("mygroup", "task2", 1, false);
        engine.task_completion_tx.send(completion2).await.unwrap();
        engine.process_task_completions().await;

        let group = engine.groups.get("mygroup").unwrap();
        // Group goes to Failed, then cascade_skip may change it to Skipped
        // Both indicate failure - check for either
        assert!(
            group.state.status == GroupStatus::Failed
                || group.state.status == GroupStatus::Skipped,
            "Expected Failed or Skipped, got {:?}",
            group.state.status
        );
    }

    #[tokio::test]
    async fn test_process_completion_removes_from_running_tasks() {
        let groups = vec![test_group("mygroup", &["task1", "task2"])];
        let mut engine = create_test_engine(groups);

        engine.running_tasks.insert("mygroup:task1".to_string());
        engine.running_tasks.insert("mygroup:task2".to_string());

        // Complete task1
        let completion = task_completion("mygroup", "task1", 0, true);
        engine.task_completion_tx.send(completion).await.unwrap();
        engine.process_task_completions().await;

        // task1 removed, task2 still running
        assert!(!engine.running_tasks.contains("mygroup:task1"));
        assert!(engine.running_tasks.contains("mygroup:task2"));
    }

    #[tokio::test]
    async fn test_process_completion_handles_pending_rerun() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        // Task is running AND pending (queued for re-run)
        engine.running_tasks.insert("mygroup:task1".to_string());
        engine.pending_tasks.insert("mygroup:task1".to_string());

        // Complete current run
        let completion = task_completion("mygroup", "task1", 0, true);
        engine.task_completion_tx.send(completion).await.unwrap();
        engine.process_task_completions().await;

        // Pending should be removed (spawned for re-run)
        assert!(!engine.pending_tasks.contains("mygroup:task1"));
        // Task should be running again
        assert!(engine.running_tasks.contains("mygroup:task1"));
    }

    #[tokio::test]
    async fn test_process_completion_multiple_completions_in_batch() {
        let groups = vec![test_group("mygroup", &["task1", "task2", "task3"])];
        let mut engine = create_test_engine(groups);

        // Set group to running
        engine.groups.get_mut("mygroup").unwrap().state.status = GroupStatus::Running;

        // All tasks running
        engine.running_tasks.insert("mygroup:task1".to_string());
        engine.running_tasks.insert("mygroup:task2".to_string());
        engine.running_tasks.insert("mygroup:task3".to_string());

        // Send all completions at once
        engine
            .task_completion_tx
            .send(task_completion("mygroup", "task1", 0, true))
            .await
            .unwrap();
        engine
            .task_completion_tx
            .send(task_completion("mygroup", "task2", 1, true))
            .await
            .unwrap();
        engine
            .task_completion_tx
            .send(task_completion("mygroup", "task3", 2, true))
            .await
            .unwrap();

        // Process all at once
        engine.process_task_completions().await;

        // All tasks should be complete
        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.tasks[0].status, ProcessStatus::Success);
        assert_eq!(group.state.tasks[1].status, ProcessStatus::Success);
        assert_eq!(group.state.tasks[2].status, ProcessStatus::Success);
        assert_eq!(group.state.status, GroupStatus::Ready);
        assert!(engine.running_tasks.is_empty());
    }

    #[tokio::test]
    async fn test_process_completion_daemon_start_signal() {
        // Special __daemon_start__ task ID for groups without tasks
        let groups = vec![test_group("mygroup", &[])]; // No tasks
        let mut engine = create_test_engine(groups);

        // Send daemon start signal
        let completion = TaskCompletion {
            task_id: "__daemon_start__".to_string(),
            group_name: "mygroup".to_string(),
            task_index: 0,
            success: true,
            status: ProcessStatus::Success,
            duration_ms: None,
            exit_code: None,
        };
        engine.task_completion_tx.send(completion).await.unwrap();

        // Process - this should queue daemon action
        engine.process_task_completions().await;

        // The daemon action is processed and queues to handle_daemon_action.
        // For groups without daemons, handle_daemon_action sets status to Ready
        // and triggers dependents. Check that the signal was processed
        // (running_tasks unchanged since __daemon_start__ isn't added to running_tasks).
        // The actual status depends on whether handle_daemon_action ran.
        let group = engine.groups.get("mygroup").unwrap();
        // Group either stays Pending (daemon action queued but not fully processed)
        // or becomes Ready (if handle_daemon_action completed)
        assert!(
            group.state.status == GroupStatus::Pending
                || group.state.status == GroupStatus::Ready,
            "Expected Pending or Ready for daemon start signal, got {:?}",
            group.state.status
        );
    }

    #[tokio::test]
    async fn test_process_completion_no_completions_is_noop() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        engine.running_tasks.insert("mygroup:task1".to_string());
        let initial_running = engine.running_tasks.clone();

        // Process with nothing in channel
        engine.process_task_completions().await;

        // Nothing should change
        assert_eq!(engine.running_tasks, initial_running);
    }

    #[tokio::test]
    async fn test_process_completion_unknown_group_ignored() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        // Send completion for non-existent group
        let completion = task_completion("nonexistent", "task1", 0, true);
        engine.task_completion_tx.send(completion).await.unwrap();

        // Should not panic
        engine.process_task_completions().await;

        // Original group unchanged
        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.status, GroupStatus::Pending);
    }

    #[tokio::test]
    async fn test_process_completion_with_dependencies_triggers_cascade() {
        // Group "a" depends on "b"
        let groups = vec![
            test_group("b", &["task1"]),
            test_group_with_deps("a", &["task1"], &["b"]),
        ];
        let mut engine = create_test_engine(groups);

        // Set up: b is running, a is waiting for b
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Running;
        engine.running_tasks.insert("b:task1".to_string());

        // a is waiting for b
        engine.dependency_resolver.mark_waiting("a");
        engine.groups.get_mut("a").unwrap().state.status = GroupStatus::Waiting;

        // b's task completes
        let completion = task_completion("b", "task1", 0, true);
        engine.task_completion_tx.send(completion).await.unwrap();
        engine.process_task_completions().await;

        // b should be Ready
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Ready
        );

        // a should have been triggered (waiting cleared, status Running or Ready)
        assert!(!engine.dependency_resolver.is_waiting("a"));
    }

    // =========================================================================
    // should_suppress() tests
    // =========================================================================

    #[test]
    fn test_should_suppress_none_allows_all() {
        assert!(!should_suppress(Silence::None, false)); // stdout allowed
        assert!(!should_suppress(Silence::None, true)); // stderr allowed
    }

    #[test]
    fn test_should_suppress_all_blocks_all() {
        assert!(should_suppress(Silence::All, false)); // stdout blocked
        assert!(should_suppress(Silence::All, true)); // stderr blocked
    }

    #[test]
    fn test_should_suppress_stdout_only() {
        assert!(should_suppress(Silence::Stdout, false)); // stdout blocked
        assert!(!should_suppress(Silence::Stdout, true)); // stderr allowed
    }

    #[test]
    fn test_should_suppress_stderr_only() {
        assert!(!should_suppress(Silence::Stderr, false)); // stdout allowed
        assert!(should_suppress(Silence::Stderr, true)); // stderr blocked
    }

    // =========================================================================
    // topological_sort() tests
    // =========================================================================

    #[test]
    fn test_topological_sort_simple() {
        let groups = vec![
            Group {
                name: "a".to_string(),
                depends_on: vec!["b".to_string()],
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                ..Default::default()
            },
        ];

        let order = topological_sort(&groups).unwrap();
        assert_eq!(order, vec!["b", "a"]);
    }

    #[test]
    fn test_topological_sort_no_deps() {
        let groups = vec![
            Group {
                name: "a".to_string(),
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                ..Default::default()
            },
        ];

        let order = topological_sort(&groups).unwrap();
        assert_eq!(order.len(), 2);
        assert!(order.contains(&"a".to_string()));
        assert!(order.contains(&"b".to_string()));
    }

    #[test]
    fn test_topological_sort_cyclic_dependency() {
        let groups = vec![
            Group {
                name: "a".to_string(),
                depends_on: vec!["b".to_string()],
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                depends_on: vec!["a".to_string()],
                ..Default::default()
            },
        ];

        let result = topological_sort(&groups);
        assert!(matches!(result, Err(DaemonError::CyclicDependency(_))));
    }

    #[test]
    fn test_topological_sort_self_dependency() {
        let groups = vec![Group {
            name: "a".to_string(),
            depends_on: vec!["a".to_string()],
            ..Default::default()
        }];

        let result = topological_sort(&groups);
        assert!(matches!(result, Err(DaemonError::CyclicDependency(_))));
    }

    #[test]
    fn test_topological_sort_diamond_dependency() {
        // Diamond: a depends on b,c; b,c depend on d
        let groups = vec![
            Group {
                name: "a".to_string(),
                depends_on: vec!["b".to_string(), "c".to_string()],
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                depends_on: vec!["d".to_string()],
                ..Default::default()
            },
            Group {
                name: "c".to_string(),
                depends_on: vec!["d".to_string()],
                ..Default::default()
            },
            Group {
                name: "d".to_string(),
                ..Default::default()
            },
        ];

        let order = topological_sort(&groups).unwrap();

        // d must come before b and c, which must come before a
        let pos_a = order.iter().position(|x| x == "a").unwrap();
        let pos_b = order.iter().position(|x| x == "b").unwrap();
        let pos_c = order.iter().position(|x| x == "c").unwrap();
        let pos_d = order.iter().position(|x| x == "d").unwrap();

        assert!(pos_d < pos_b, "d must come before b");
        assert!(pos_d < pos_c, "d must come before c");
        assert!(pos_b < pos_a, "b must come before a");
        assert!(pos_c < pos_a, "c must come before a");
    }

    #[test]
    fn test_topological_sort_deep_chain() {
        // Linear chain: a -> b -> c -> d -> e
        let groups = vec![
            Group {
                name: "a".to_string(),
                depends_on: vec!["b".to_string()],
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                depends_on: vec!["c".to_string()],
                ..Default::default()
            },
            Group {
                name: "c".to_string(),
                depends_on: vec!["d".to_string()],
                ..Default::default()
            },
            Group {
                name: "d".to_string(),
                depends_on: vec!["e".to_string()],
                ..Default::default()
            },
            Group {
                name: "e".to_string(),
                ..Default::default()
            },
        ];

        let order = topological_sort(&groups).unwrap();
        assert_eq!(order, vec!["e", "d", "c", "b", "a"]);
    }

    #[test]
    fn test_topological_sort_missing_dependency() {
        // Group depends on non-existent group - function visits it and includes
        // it in the result (graceful handling, doesn't error)
        let groups = vec![Group {
            name: "a".to_string(),
            depends_on: vec!["nonexistent".to_string()],
            ..Default::default()
        }];

        let order = topological_sort(&groups).unwrap();
        // The missing dependency is visited and added to result before "a"
        assert_eq!(order, vec!["nonexistent", "a"]);
    }

    #[test]
    fn test_topological_sort_complex_cycle() {
        // a -> b -> c -> a (3-node cycle)
        let groups = vec![
            Group {
                name: "a".to_string(),
                depends_on: vec!["b".to_string()],
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                depends_on: vec!["c".to_string()],
                ..Default::default()
            },
            Group {
                name: "c".to_string(),
                depends_on: vec!["a".to_string()],
                ..Default::default()
            },
        ];

        let result = topological_sort(&groups);
        assert!(matches!(result, Err(DaemonError::CyclicDependency(_))));
    }

    // =========================================================================
    // compute_config_changes() tests
    // =========================================================================

    #[test]
    fn test_compute_config_changes_no_changes() {
        let groups = vec![
            Group {
                name: "a".to_string(),
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                ..Default::default()
            },
        ];

        let (added, removed, modified) = compute_config_changes(&groups, &groups);
        assert!(added.is_empty());
        assert!(removed.is_empty());
        assert!(modified.is_empty());
    }

    #[test]
    fn test_compute_config_changes_added_groups() {
        let old = vec![Group {
            name: "a".to_string(),
            ..Default::default()
        }];
        let new = vec![
            Group {
                name: "a".to_string(),
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                ..Default::default()
            },
        ];

        let (added, removed, modified) = compute_config_changes(&old, &new);
        assert_eq!(added, vec!["b"]);
        assert!(removed.is_empty());
        assert!(modified.is_empty());
    }

    #[test]
    fn test_compute_config_changes_removed_groups() {
        let old = vec![
            Group {
                name: "a".to_string(),
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                ..Default::default()
            },
        ];
        let new = vec![Group {
            name: "a".to_string(),
            ..Default::default()
        }];

        let (added, removed, modified) = compute_config_changes(&old, &new);
        assert!(added.is_empty());
        assert_eq!(removed, vec!["b"]);
        assert!(modified.is_empty());
    }

    #[test]
    fn test_compute_config_changes_modified_patterns() {
        let old = vec![Group {
            name: "a".to_string(),
            patterns: vec!["*.rs".to_string()],
            ..Default::default()
        }];
        let new = vec![Group {
            name: "a".to_string(),
            patterns: vec!["*.go".to_string()],
            ..Default::default()
        }];

        let (added, removed, modified) = compute_config_changes(&old, &new);
        assert!(added.is_empty());
        assert!(removed.is_empty());
        assert_eq!(modified, vec!["a"]);
    }

    #[test]
    fn test_compute_config_changes_modified_depends_on() {
        let old = vec![Group {
            name: "a".to_string(),
            depends_on: vec!["b".to_string()],
            ..Default::default()
        }];
        let new = vec![Group {
            name: "a".to_string(),
            depends_on: vec!["c".to_string()],
            ..Default::default()
        }];

        let (added, removed, modified) = compute_config_changes(&old, &new);
        assert!(added.is_empty());
        assert!(removed.is_empty());
        assert_eq!(modified, vec!["a"]);
    }

    #[test]
    fn test_compute_config_changes_all_operations() {
        let old = vec![
            Group {
                name: "keep".to_string(),
                ..Default::default()
            },
            Group {
                name: "remove".to_string(),
                ..Default::default()
            },
            Group {
                name: "modify".to_string(),
                patterns: vec!["old".to_string()],
                ..Default::default()
            },
        ];
        let new = vec![
            Group {
                name: "keep".to_string(),
                ..Default::default()
            },
            Group {
                name: "add".to_string(),
                ..Default::default()
            },
            Group {
                name: "modify".to_string(),
                patterns: vec!["new".to_string()],
                ..Default::default()
            },
        ];

        let (added, removed, modified) = compute_config_changes(&old, &new);
        assert_eq!(added, vec!["add"]);
        assert_eq!(removed, vec!["remove"]);
        assert_eq!(modified, vec!["modify"]);
    }

    #[test]
    fn test_compute_config_changes_empty_to_groups() {
        let old: Vec<Group> = vec![];
        let new = vec![
            Group {
                name: "a".to_string(),
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                ..Default::default()
            },
        ];

        let (added, removed, modified) = compute_config_changes(&old, &new);
        assert_eq!(added.len(), 2);
        assert!(added.contains(&"a".to_string()));
        assert!(added.contains(&"b".to_string()));
        assert!(removed.is_empty());
        assert!(modified.is_empty());
    }

    #[test]
    fn test_compute_config_changes_groups_to_empty() {
        let old = vec![
            Group {
                name: "a".to_string(),
                ..Default::default()
            },
            Group {
                name: "b".to_string(),
                ..Default::default()
            },
        ];
        let new: Vec<Group> = vec![];

        let (added, removed, modified) = compute_config_changes(&old, &new);
        assert!(added.is_empty());
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"a".to_string()));
        assert!(removed.contains(&"b".to_string()));
        assert!(modified.is_empty());
    }

    // =========================================================================
    // calculate_group_status_from_tasks() tests
    // =========================================================================

    #[test]
    fn test_calculate_group_status_returns_none_when_tasks_running() {
        let tasks = vec![
            ProcessState {
                name: "task1".to_string(),
                status: ProcessStatus::Success,
                ..Default::default()
            },
            ProcessState {
                name: "task2".to_string(),
                status: ProcessStatus::Running,
                ..Default::default()
            },
        ];

        let result = calculate_group_status_from_tasks(&tasks, true);
        assert!(result.is_none());
    }

    #[test]
    fn test_calculate_group_status_ready_when_all_success() {
        let tasks = vec![
            ProcessState {
                name: "task1".to_string(),
                status: ProcessStatus::Success,
                ..Default::default()
            },
            ProcessState {
                name: "task2".to_string(),
                status: ProcessStatus::Success,
                ..Default::default()
            },
        ];

        let result = calculate_group_status_from_tasks(&tasks, false);
        assert_eq!(result, Some(GroupStatus::Ready));
    }

    #[test]
    fn test_calculate_group_status_failed_when_any_failed() {
        let tasks = vec![
            ProcessState {
                name: "task1".to_string(),
                status: ProcessStatus::Success,
                ..Default::default()
            },
            ProcessState {
                name: "task2".to_string(),
                status: ProcessStatus::Failed,
                ..Default::default()
            },
        ];

        let result = calculate_group_status_from_tasks(&tasks, false);
        assert_eq!(result, Some(GroupStatus::Failed));
    }

    #[test]
    fn test_calculate_group_status_ready_with_empty_tasks() {
        let tasks: Vec<ProcessState> = vec![];

        let result = calculate_group_status_from_tasks(&tasks, false);
        assert_eq!(result, Some(GroupStatus::Ready));
    }

    #[test]
    fn test_calculate_group_status_ready_with_pending_and_success() {
        // Pending tasks don't count as failed
        let tasks = vec![
            ProcessState {
                name: "task1".to_string(),
                status: ProcessStatus::Success,
                ..Default::default()
            },
            ProcessState {
                name: "task2".to_string(),
                status: ProcessStatus::Pending,
                ..Default::default()
            },
        ];

        let result = calculate_group_status_from_tasks(&tasks, false);
        assert_eq!(result, Some(GroupStatus::Ready));
    }

    // =========================================================================
    // filter_affected_to_roots() tests
    // =========================================================================

    #[test]
    fn test_filter_roots_single_group_no_deps() {
        let affected = vec!["a".to_string()];
        let get_deps = |_: &str| Vec::new();

        let roots = filter_affected_to_roots(&affected, get_deps);
        assert_eq!(roots, vec!["a"]);
    }

    #[test]
    fn test_filter_roots_independent_groups() {
        let affected = vec!["a".to_string(), "b".to_string()];
        let get_deps = |_: &str| Vec::new();

        let roots = filter_affected_to_roots(&affected, get_deps);
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&"a".to_string()));
        assert!(roots.contains(&"b".to_string()));
    }

    #[test]
    fn test_filter_roots_dependent_filtered_out() {
        // a depends on b, both affected -> only b is root
        let affected = vec!["a".to_string(), "b".to_string()];
        let get_deps = |g: &str| {
            if g == "a" {
                vec!["b".to_string()]
            } else {
                vec![]
            }
        };

        let roots = filter_affected_to_roots(&affected, get_deps);
        assert_eq!(roots, vec!["b"]);
    }

    #[test]
    fn test_filter_roots_chain_only_first() {
        // a -> b -> c, all affected -> only c is root
        let affected = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let get_deps = |g: &str| match g {
            "a" => vec!["b".to_string()],
            "b" => vec!["c".to_string()],
            _ => vec![],
        };

        let roots = filter_affected_to_roots(&affected, get_deps);
        assert_eq!(roots, vec!["c"]);
    }

    #[test]
    fn test_filter_roots_diamond_both_middle() {
        // Diamond: a -> b,c -> d. If b,c,d affected, only d is root
        let affected = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        let get_deps = |g: &str| match g {
            "a" => vec!["b".to_string(), "c".to_string()],
            "b" => vec!["d".to_string()],
            "c" => vec!["d".to_string()],
            _ => vec![],
        };

        let roots = filter_affected_to_roots(&affected, get_deps);
        assert_eq!(roots, vec!["d"]);
    }

    #[test]
    fn test_filter_roots_partial_chain() {
        // a -> b -> c, only a and c affected -> both are roots (b not affected)
        let affected = vec!["a".to_string(), "c".to_string()];
        let get_deps = |g: &str| match g {
            "a" => vec!["b".to_string()],
            "b" => vec!["c".to_string()],
            _ => vec![],
        };

        let roots = filter_affected_to_roots(&affected, get_deps);
        // a's dependency (b) is not affected, so a is a root
        // c has no dependencies, so c is a root
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&"a".to_string()));
        assert!(roots.contains(&"c".to_string()));
    }

    #[test]
    fn test_filter_roots_unaffected_dependency() {
        // a depends on b, but only a is affected -> a is root
        let affected = vec!["a".to_string()];
        let get_deps = |g: &str| {
            if g == "a" {
                vec!["b".to_string()]
            } else {
                vec![]
            }
        };

        let roots = filter_affected_to_roots(&affected, get_deps);
        assert_eq!(roots, vec!["a"]);
    }

    #[test]
    fn test_filter_roots_empty_affected() {
        let affected: Vec<String> = vec![];
        let get_deps = |_: &str| Vec::new();

        let roots = filter_affected_to_roots(&affected, get_deps);
        assert!(roots.is_empty());
    }

    // =========================================================================
    // startup() integration tests
    // =========================================================================

    #[tokio::test]
    async fn test_startup_group_no_deps_starts_immediately() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        engine.startup().await.unwrap();

        // Group should be Running (tasks spawned)
        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.status, GroupStatus::Running);
        // Task should be running
        assert!(engine.running_tasks.contains("mygroup:task1"));
    }

    #[tokio::test]
    async fn test_startup_group_with_deps_waits() {
        // a depends on b
        let groups = vec![
            test_group("b", &["task1"]),
            test_group_with_deps("a", &["task1"], &["b"]),
        ];
        let mut engine = create_test_engine(groups);

        engine.startup().await.unwrap();

        // b should be Running (no deps)
        let group_b = engine.groups.get("b").unwrap();
        assert_eq!(group_b.state.status, GroupStatus::Running);

        // a should be Waiting (depends on b)
        let group_a = engine.groups.get("a").unwrap();
        assert_eq!(group_a.state.status, GroupStatus::Waiting);
        assert!(engine.dependency_resolver.is_waiting("a"));
    }

    #[tokio::test]
    async fn test_startup_group_no_tasks_no_daemons_ready() {
        // Group with no tasks and no daemons should be Ready immediately
        let groups = vec![Group {
            name: "empty".to_string(),
            tasks: vec![],
            patterns: vec!["*.test".to_string()],
            ..Default::default()
        }];
        let mut engine = create_test_engine(groups);

        engine.startup().await.unwrap();

        // Group should be Ready (nothing to do)
        let group = engine.groups.get("empty").unwrap();
        assert_eq!(group.state.status, GroupStatus::Ready);
    }

    #[tokio::test]
    async fn test_startup_chain_deps_execution_order() {
        // c -> b -> a (a depends on b, b depends on c)
        let groups = vec![
            test_group("c", &["task1"]),
            test_group_with_deps("b", &["task1"], &["c"]),
            test_group_with_deps("a", &["task1"], &["b"]),
        ];
        let mut engine = create_test_engine(groups);

        engine.startup().await.unwrap();

        // c should be Running (no deps)
        assert_eq!(
            engine.groups.get("c").unwrap().state.status,
            GroupStatus::Running
        );

        // b should be Waiting for c
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Waiting
        );
        assert!(engine
            .dependency_resolver
            .waiting_for("b")
            .unwrap()
            .contains("c"));

        // a should be Waiting for b
        assert_eq!(
            engine.groups.get("a").unwrap().state.status,
            GroupStatus::Waiting
        );
        assert!(engine
            .dependency_resolver
            .waiting_for("a")
            .unwrap()
            .contains("b"));
    }

    #[tokio::test]
    async fn test_startup_diamond_deps() {
        // Diamond: a -> b,c -> d
        let groups = vec![
            test_group("d", &["task1"]),
            test_group_with_deps("b", &["task1"], &["d"]),
            test_group_with_deps("c", &["task1"], &["d"]),
            test_group_with_deps("a", &["task1"], &["b", "c"]),
        ];
        let mut engine = create_test_engine(groups);

        engine.startup().await.unwrap();

        // d should be Running (no deps)
        assert_eq!(
            engine.groups.get("d").unwrap().state.status,
            GroupStatus::Running
        );

        // b and c should be Waiting for d
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Waiting
        );
        assert_eq!(
            engine.groups.get("c").unwrap().state.status,
            GroupStatus::Waiting
        );

        // a should be Waiting for both b and c
        assert_eq!(
            engine.groups.get("a").unwrap().state.status,
            GroupStatus::Waiting
        );
        let waiting_for_a = engine.dependency_resolver.waiting_for("a").unwrap();
        assert!(waiting_for_a.contains("b"));
        assert!(waiting_for_a.contains("c"));
    }

    #[tokio::test]
    async fn test_startup_only_on_change_only_tasks_skips_to_daemon() {
        // Group with no startup tasks (only on_change_only or none) should skip to daemon handling
        // Since TaskCommand doesn't expose on_change_only directly, we test with an empty task list
        let groups = vec![Group {
            name: "mygroup".to_string(),
            tasks: vec![], // No tasks means it goes straight to daemon handling
            patterns: vec!["*.test".to_string()],
            ..Default::default()
        }];
        let mut engine = create_test_engine(groups);

        engine.startup().await.unwrap();

        // Group should be Ready (no tasks, no daemons)
        let group = engine.groups.get("mygroup").unwrap();
        assert_eq!(group.state.status, GroupStatus::Ready);
        // No tasks should be running
        assert!(engine.running_tasks.is_empty());
    }

    #[tokio::test]
    async fn test_startup_sets_daemon_status_running() {
        let groups = vec![test_group("mygroup", &["task1"])];
        let mut engine = create_test_engine(groups);

        assert_eq!(engine.state.status, DaemonStatus::Starting);

        engine.startup().await.unwrap();

        assert_eq!(engine.state.status, DaemonStatus::Running);
    }

    // =========================================================================
    // trigger_dependents() and cascade tests
    // =========================================================================

    #[tokio::test]
    async fn test_trigger_dependents_starts_waiting_group() {
        // b depends on a
        let groups = vec![
            test_group("a", &["task1"]),
            test_group_with_deps("b", &["task1"], &["a"]),
        ];
        let mut engine = create_test_engine(groups);

        // Simulate: a is Ready, b is Waiting for a
        // Note: mark_waiting must be called BEFORE setting a to Ready,
        // otherwise the resolver sees all deps satisfied
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("b");
        engine.groups.get_mut("a").unwrap().state.status = GroupStatus::Ready;

        // Trigger dependents of a
        engine.trigger_dependents("a");

        // b should no longer be waiting
        assert!(!engine.dependency_resolver.is_waiting("b"));
        // b should be Running (tasks spawned) or have tasks in running_tasks
        assert!(
            engine.groups.get("b").unwrap().state.status == GroupStatus::Running
                || engine.running_tasks.contains("b:task1")
        );
    }

    #[tokio::test]
    async fn test_trigger_dependents_multiple_dependents() {
        // b and c both depend on a
        let groups = vec![
            test_group("a", &["task1"]),
            test_group_with_deps("b", &["task1"], &["a"]),
            test_group_with_deps("c", &["task1"], &["a"]),
        ];
        let mut engine = create_test_engine(groups);

        // Simulate: a is Ready, b and c are Waiting for a
        // Note: mark_waiting must be called BEFORE setting a to Ready
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("b");
        engine.groups.get_mut("c").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("c");
        engine.groups.get_mut("a").unwrap().state.status = GroupStatus::Ready;

        // Trigger dependents of a
        engine.trigger_dependents("a");

        // Both b and c should no longer be waiting
        assert!(!engine.dependency_resolver.is_waiting("b"));
        assert!(!engine.dependency_resolver.is_waiting("c"));
    }

    #[tokio::test]
    async fn test_trigger_dependents_partial_deps_satisfied() {
        // a depends on both b and c
        let groups = vec![
            test_group("b", &["task1"]),
            test_group("c", &["task1"]),
            test_group_with_deps("a", &["task1"], &["b", "c"]),
        ];
        let mut engine = create_test_engine(groups);

        // Simulate: b is Ready, c is Running, a is Waiting for both
        // Note: mark_waiting must be called BEFORE setting b to Ready
        engine.groups.get_mut("c").unwrap().state.status = GroupStatus::Running;
        engine.groups.get_mut("a").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("a");
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Ready;
        engine.dependency_resolver.set_status("b", GroupStatus::Ready);

        // Trigger dependents of b
        engine.trigger_dependents("b");

        // a should still be waiting (c not ready yet)
        assert!(engine.dependency_resolver.is_waiting("a"));
        // But b should be removed from waiting set
        let waiting_for_a = engine.dependency_resolver.waiting_for("a").unwrap();
        assert!(!waiting_for_a.contains("b"));
        assert!(waiting_for_a.contains("c"));
    }

    #[tokio::test]
    async fn test_cascade_skip_propagates_failure() {
        // c -> b -> a
        let groups = vec![
            test_group("c", &["task1"]),
            test_group_with_deps("b", &["task1"], &["c"]),
            test_group_with_deps("a", &["task1"], &["b"]),
        ];
        let mut engine = create_test_engine(groups);

        // Simulate: b is waiting for c, a is waiting for b
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("b");
        engine.groups.get_mut("a").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("a");

        // c fails - cascade skip
        engine.cascade_skip("c");

        // c should be Skipped
        assert_eq!(
            engine.groups.get("c").unwrap().state.status,
            GroupStatus::Skipped
        );
        // b should be Skipped (was waiting for c)
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Skipped
        );
        // a should be Skipped (was waiting for b)
        assert_eq!(
            engine.groups.get("a").unwrap().state.status,
            GroupStatus::Skipped
        );
    }

    #[tokio::test]
    async fn test_cascade_skip_diamond_pattern() {
        // Diamond: a -> b,c -> d
        let groups = vec![
            test_group("d", &["task1"]),
            test_group_with_deps("b", &["task1"], &["d"]),
            test_group_with_deps("c", &["task1"], &["d"]),
            test_group_with_deps("a", &["task1"], &["b", "c"]),
        ];
        let mut engine = create_test_engine(groups);

        // Simulate: b,c waiting for d; a waiting for b,c
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("b");
        engine.groups.get_mut("c").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("c");
        engine.groups.get_mut("a").unwrap().state.status = GroupStatus::Waiting;
        engine.dependency_resolver.mark_waiting("a");

        // d fails - cascade skip
        engine.cascade_skip("d");

        // All should be Skipped
        assert_eq!(
            engine.groups.get("d").unwrap().state.status,
            GroupStatus::Skipped
        );
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Skipped
        );
        assert_eq!(
            engine.groups.get("c").unwrap().state.status,
            GroupStatus::Skipped
        );
        assert_eq!(
            engine.groups.get("a").unwrap().state.status,
            GroupStatus::Skipped
        );
    }

    #[tokio::test]
    async fn test_cascade_skip_not_waiting_groups_unaffected() {
        // b depends on a, but b is not waiting (already running)
        let groups = vec![
            test_group("a", &["task1"]),
            test_group_with_deps("b", &["task1"], &["a"]),
        ];
        let mut engine = create_test_engine(groups);

        // b is Running, not Waiting (not marked as waiting in resolver)
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Running;

        // a fails
        engine.cascade_skip("a");

        // a should be Skipped
        assert_eq!(
            engine.groups.get("a").unwrap().state.status,
            GroupStatus::Skipped
        );
        // b should still be Running (not affected because not waiting)
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Running
        );
    }

    // =========================================================================
    // Full flow integration tests
    // =========================================================================

    #[tokio::test]
    async fn test_full_startup_completion_flow() {
        // Simple flow: a depends on b, both complete successfully
        let groups = vec![
            test_group("b", &["task1"]),
            test_group_with_deps("a", &["task1"], &["b"]),
        ];
        let mut engine = create_test_engine(groups);

        // Start up
        engine.startup().await.unwrap();

        // b running, a waiting
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Running
        );
        assert_eq!(
            engine.groups.get("a").unwrap().state.status,
            GroupStatus::Waiting
        );

        // b's task completes
        let completion = task_completion("b", "task1", 0, true);
        engine.task_completion_tx.send(completion).await.unwrap();
        engine.process_task_completions().await;

        // b should be Ready, a should be Running
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Ready
        );
        // a should no longer be waiting and should be running
        assert!(!engine.dependency_resolver.is_waiting("a"));
        assert!(
            engine.groups.get("a").unwrap().state.status == GroupStatus::Running
                || engine.running_tasks.contains("a:task1")
        );
    }

    #[tokio::test]
    async fn test_full_startup_failure_cascade_flow() {
        // a depends on b, b fails
        let groups = vec![
            test_group("b", &["task1"]),
            test_group_with_deps("a", &["task1"], &["b"]),
        ];
        let mut engine = create_test_engine(groups);

        // Start up
        engine.startup().await.unwrap();

        // b's task fails
        let completion = task_completion("b", "task1", 0, false);
        engine.task_completion_tx.send(completion).await.unwrap();
        engine.process_task_completions().await;

        // b should be Failed or Skipped
        assert!(engine.groups.get("b").unwrap().state.status == GroupStatus::Failed
            || engine.groups.get("b").unwrap().state.status == GroupStatus::Skipped);

        // a should be Skipped
        assert_eq!(
            engine.groups.get("a").unwrap().state.status,
            GroupStatus::Skipped
        );
    }

    // =========================================================================
    // cascade_daemon_restart tests
    // =========================================================================

    #[tokio::test]
    async fn test_dependency_resolver_returns_dependents() {
        // b depends on a - when a restarts, b should restart
        let groups = vec![
            test_group("a", &["task1"]),
            test_group_with_deps("b", &["task1"], &["a"]),
        ];
        let engine = create_test_engine(groups);

        // Verify the resolver returns b as a dependent of a
        let dependents = engine.dependency_resolver.get_dependents("a");
        assert_eq!(dependents, vec!["b".to_string()]);

        // Verify the forward dependency is also correct
        let deps = engine.dependency_resolver.get_dependencies("b");
        assert_eq!(deps, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn test_dependency_resolver_chain_dependents() {
        // c depends on b, b depends on a
        // When a restarts, both b and c should be affected
        let groups = vec![
            test_group("a", &["task1"]),
            test_group_with_deps("b", &["task1"], &["a"]),
            test_group_with_deps("c", &["task1"], &["b"]),
        ];
        let engine = create_test_engine(groups);

        // a's immediate dependents should be just b
        let dependents_of_a = engine.dependency_resolver.get_dependents("a");
        assert_eq!(dependents_of_a, vec!["b".to_string()]);

        // b's immediate dependents should be just c
        let dependents_of_b = engine.dependency_resolver.get_dependents("b");
        assert_eq!(dependents_of_b, vec!["c".to_string()]);
    }

    #[tokio::test]
    async fn test_trigger_dependents_on_restart_does_nothing_for_ready_groups() {
        // This test documents the current behavior: trigger_dependents only affects
        // groups that are in waiting state. For restart cascading, use cascade_daemon_restart.
        let groups = vec![
            test_group("a", &["task1"]),
            test_group_with_deps("b", &["task1"], &["a"]),
        ];
        let mut engine = create_test_engine(groups);

        // Both groups are already Ready (simulating post-startup state)
        engine.groups.get_mut("a").unwrap().state.status = GroupStatus::Ready;
        engine.dependency_resolver.set_status("a", GroupStatus::Ready);
        engine.groups.get_mut("b").unwrap().state.status = GroupStatus::Ready;
        engine.dependency_resolver.set_status("b", GroupStatus::Ready);

        // Call trigger_dependents (simulating what happens after a restart)
        engine.trigger_dependents("a");

        // b should still be Ready (not Running) because it wasn't waiting
        // The cascade for restarts should go through cascade_daemon_restart instead
        assert_eq!(
            engine.groups.get("b").unwrap().state.status,
            GroupStatus::Ready
        );
    }
}
