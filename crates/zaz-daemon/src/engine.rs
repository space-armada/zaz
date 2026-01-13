//! Core orchestration engine for zaz.
//!
//! The engine ties together configuration, file watching, and process management.

use crate::api::LogLine;
use crate::state::{
    DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus,
};
use crate::{ApiResponse, DaemonError};
use indexmap::IndexMap;
use std::collections::HashMap;
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

    /// Reverse dependency map: group_name -> groups that depend on it.
    /// Used to efficiently find dependents when a group completes.
    dependents: HashMap<String, Vec<String>>,

    /// Groups waiting for dependencies: group_name -> deps still waiting for.
    /// When all deps complete, the group can start.
    waiting_groups: HashMap<String, std::collections::HashSet<String>>,

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

        // Build reverse dependency map (dependents)
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for group in &config.groups {
            for dep in &group.depends_on {
                dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(group.name.clone());
            }
        }

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

        // Create log store with verbose callback if enabled
        let log_store = if verbose_output {
            crate::log_store::LogStore::new().with_verbose_callback(|log| {
                println!("[{}] {}", log.process, log.content);
            })
        } else {
            crate::log_store::LogStore::new()
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
            dependents,
            waiting_groups: HashMap::new(),
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

        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_root(config_dir.to_path_buf());

        // Track groups that need immediate daemon startup (no runnable tasks at startup
        // AND all dependencies satisfied)
        let mut groups_needing_daemon_start: Vec<String> = Vec::new();

        for group_name in &self.execution_order.clone() {
            let deps = self.get_group_dependencies(group_name);

            // Find dependencies that are not yet Ready
            let unready_deps: std::collections::HashSet<String> = deps
                .into_iter()
                .filter(|d| !self.is_group_ready(d))
                .collect();

            // Has unready dependencies - mark as Waiting and track what we're waiting for
            if !unready_deps.is_empty() {
                tracing::debug!(
                    group = %group_name,
                    waiting_for = ?unready_deps,
                    "group waiting for dependencies"
                );
                self.set_group_status(group_name, GroupStatus::Waiting);
                self.waiting_groups.insert(group_name.clone(), unready_deps);
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
                self.spawn_group_tasks(group_name, &context, false);
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
        let affected_set: std::collections::HashSet<&String> = affected_groups.iter().collect();
        let roots: Vec<String> = affected_groups
            .iter()
            .filter(|g| {
                let deps = self.get_group_dependencies(g);
                // Keep this group only if none of its dependencies are also affected
                // TODO(ripta): Is this the right call?
                !deps.iter().any(|dep| affected_set.contains(dep))
            })
            .cloned()
            .collect();

        tracing::info!(
            files = changed_paths.len(),
            affected = ?affected_groups,
            roots = ?roots,
            "processing file changes (dependents will cascade)"
        );

        // Build variable context for command expansion
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_files(changed_paths)
            .with_root(config_dir.to_path_buf());

        for group_name in roots {
            self.spawn_group_tasks(&group_name, &context, true);
        }

        Ok(())
    }

    /// Spawn all tasks in a group. Each task runs in parallel with per-task deduplication.
    fn spawn_group_tasks(
        &mut self,
        group_name: &str,
        context: &Context,
        is_change_triggered: bool,
    ) {
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
            // Skip on_change_only tasks during startup
            if task.on_change_only && !is_change_triggered {
                tracing::debug!(task = %task.name(), "skipping on_change_only task during startup");
                continue;
            }

            // Expand variables in command
            let expander = zaz_vars::Expander::new(context);
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

                if !has_running_tasks {
                    // No tasks running - update group status based on task results
                    let any_failed = group
                        .state
                        .tasks
                        .iter()
                        .any(|t| t.status == ProcessStatus::Failed);

                    let new_status = if any_failed {
                        GroupStatus::Failed
                    } else {
                        GroupStatus::Ready
                    };

                    group.state.status = new_status;

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

            // Trigger dependents after group completes (with or without daemons)
            self.trigger_dependents(&group_name);
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

        // Cascade daemon restart to dependents (only for restarts, not first start)
        if !should_start {
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
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_root(config_dir.to_path_buf());
        self.spawn_group_tasks(group_name, &context, false);
        Ok(())
    }

    /// Restart all groups.
    ///
    /// Spawns all group executions asynchronously. Returns immediately.
    pub fn restart_all(&mut self) -> Result<(), DaemonError> {
        tracing::info!("restarting all groups");
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_root(config_dir.to_path_buf());
        for group_name in &self.execution_order.clone() {
            self.spawn_group_tasks(group_name, &context, false);
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
        let (added, removed, modified) = self.compute_config_changes(&new_config);

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

        // 6b. Rebuild reverse dependency map
        self.dependents.clear();
        for group in &self.config.groups {
            for dep in &group.depends_on {
                self.dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(group.name.clone());
            }
        }

        // 6c. Clear waiting groups (will be repopulated on next startup/run)
        self.waiting_groups.clear();

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
    fn compute_config_changes(
        &self,
        new_config: &Config,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        use std::collections::HashSet;

        let old_names: HashSet<_> = self.config.groups.iter().map(|g| &g.name).collect();
        let new_names: HashSet<_> = new_config.groups.iter().map(|g| &g.name).collect();

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
        for new_group in &new_config.groups {
            if let Some(old_group) = self.config.groups.iter().find(|g| g.name == new_group.name) {
                let old_json = serde_json::to_string(old_group).unwrap_or_default();
                let new_json = serde_json::to_string(new_group).unwrap_or_default();
                if old_json != new_json {
                    modified.push(new_group.name.clone());
                }
            }
        }

        (added, removed, modified)
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

        // Build variable context
        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_files(vec![])
            .with_root(config_dir.to_path_buf());

        // Check if it's a task
        let group = self.groups.get(group_name).unwrap();
        if let Some((task_idx, task_config)) = group
            .config
            .tasks
            .iter()
            .enumerate()
            .find(|(_, t)| t.name() == process_name)
        {
            tracing::info!(group = group_name, task = process_name, "restarting task");

            let expander = zaz_vars::Expander::new(&context);
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
            ApiRequest::GetLogs { name, lines } => {
                let logs = self.get_logs(&name, lines);
                ApiResponse::Logs { name, lines: logs }
            }
            ApiRequest::SubscribeLogs { name } => {
                // Return current logs; caller should use subscribe_logs() for streaming
                let logs = self.get_logs(&name, Some(100));
                ApiResponse::Logs { name, lines: logs }
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
        self.groups
            .get(group_name)
            .map(|g| g.config.depends_on.clone())
            .unwrap_or_default()
    }

    /// Check if a group has reached Ready status.
    fn is_group_ready(&self, group_name: &str) -> bool {
        self.groups
            .get(group_name)
            .map(|g| g.state.status == GroupStatus::Ready)
            .unwrap_or(false)
    }

    /// Check if a group has failed or been skipped.
    fn is_group_failed_or_skipped(&self, group_name: &str) -> bool {
        self.groups
            .get(group_name)
            .map(|g| {
                g.state.status == GroupStatus::Failed || g.state.status == GroupStatus::Skipped
            })
            .unwrap_or(false)
    }

    /// Set a group's status.
    fn set_group_status(&mut self, group_name: &str, status: GroupStatus) {
        if let Some(group) = self.groups.get_mut(group_name) {
            group.state.status = status;
        }
    }

    /// Check if any dependency of a group has failed or been skipped.
    fn any_dependency_failed(&self, group_name: &str) -> bool {
        let deps = self.get_group_dependencies(group_name);
        deps.iter().any(|dep| self.is_group_failed_or_skipped(dep))
    }

    /// Trigger dependents of a completed group.
    ///
    /// Called when a group successfully completes (reaches Ready status).
    /// This checks all groups that depend on the completed group and starts
    /// them if all their dependencies are now satisfied.
    fn trigger_dependents(&mut self, completed_group: &str) {
        let Some(dependents) = self.dependents.get(completed_group).cloned() else {
            return;
        };

        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_root(config_dir.to_path_buf());

        for dependent in dependents {
            // Check if this dependent is in waiting_groups (waiting for dependencies)
            if let Some(waiting_for) = self.waiting_groups.get_mut(&dependent) {
                waiting_for.remove(completed_group);

                if waiting_for.is_empty() {
                    // All dependencies now satisfied - remove from waiting
                    self.waiting_groups.remove(&dependent);

                    // Check if any dependency failed
                    if self.any_dependency_failed(&dependent) {
                        tracing::info!(
                            group = %dependent,
                            "skipping group due to failed dependency"
                        );
                        self.cascade_skip(&dependent);
                    } else {
                        // Start the group
                        tracing::info!(
                            group = %dependent,
                            triggered_by = %completed_group,
                            "starting group after dependency completed"
                        );
                        self.start_waiting_group(&dependent, &context);
                    }
                }
            }
        }
    }

    /// Start a group that was waiting for dependencies.
    fn start_waiting_group(&mut self, group_name: &str, context: &Context) {
        let has_startup_tasks = self
            .groups
            .get(group_name)
            .map(|g| g.config.tasks.iter().any(|t| !t.on_change_only))
            .unwrap_or(false);

        if has_startup_tasks {
            self.spawn_group_tasks(group_name, context, false);
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
        self.set_group_status(group_name, GroupStatus::Skipped);

        let Some(dependents) = self.dependents.get(group_name).cloned() else {
            return;
        };

        for dependent in dependents {
            if let Some(waiting_for) = self.waiting_groups.get_mut(&dependent) {
                waiting_for.remove(group_name);

                if waiting_for.is_empty() {
                    self.waiting_groups.remove(&dependent);
                    // Recursively skip this dependent
                    self.cascade_skip(&dependent);
                }
            }
        }
    }

    /// Cascade restart to dependent groups.
    ///
    /// When a group is restarted, this propagates the restart to all dependent
    /// groups (transitively). For each dependent:
    /// - If it has startup tasks: spawn those tasks (cascade continues when they complete)
    /// - If it has no tasks but has daemons: signal daemons directly and continue cascade
    async fn cascade_daemon_restart(&mut self, source_group: &str) -> Result<(), DaemonError> {
        let Some(dependents) = self.dependents.get(source_group).cloned() else {
            return Ok(());
        };

        let config_dir = self.config_path.parent().unwrap_or(Path::new("."));
        let context = Context::new()
            .with_variables(self.config.variables.clone())
            .with_root(config_dir.to_path_buf());

        for dependent in dependents {
            // Only cascade if the dependent has daemons that are already running
            let should_cascade = self
                .groups
                .get(&dependent)
                .map(|g| g.daemons_started && !g.daemons.is_empty())
                .unwrap_or(false);

            if should_cascade {
                // Check if dependent has startup tasks that need to run first
                let has_startup_tasks = self
                    .groups
                    .get(&dependent)
                    .map(|g| g.config.tasks.iter().any(|t| !t.on_change_only))
                    .unwrap_or(false);

                if has_startup_tasks {
                    tracing::info!(
                        from = %source_group,
                        to = %dependent,
                        "cascading restart: spawning tasks for dependent group"
                    );

                    // Spawn tasks - when they complete, daemons will be signaled
                    // and cascade will continue through process_task_completions
                    self.spawn_group_tasks(&dependent, &context, false);
                } else {
                    tracing::info!(
                        from = %source_group,
                        to = %dependent,
                        "cascading restart: signaling daemons directly (no tasks)"
                    );

                    // No tasks - signal daemons directly
                    if let Err(e) = self.signal_group_daemons(&dependent) {
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

    /// Signal all daemons in a group to restart.
    fn signal_group_daemons(&mut self, group_name: &str) -> Result<(), DaemonError> {
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
    use zaz_config::{Group, Silence};

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
}
