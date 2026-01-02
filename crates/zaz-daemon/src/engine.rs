//! Core orchestration engine for zaz.
//!
//! The engine ties together configuration, file watching, and process management.

use crate::api::LogLine;
use crate::state::{
    DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus,
};
use crate::{ApiResponse, DaemonError};
use indexmap::IndexMap;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use zaz_config::{Config, Group};
use zaz_process::{Daemon, Executor, OutputLine, TaskRunner};
use zaz_vars::Context;
use zaz_watch::{FileEvent, PatternSet, Watcher, WatcherConfig};

/// Maximum number of log lines to store per process.
const MAX_LOG_LINES: usize = 10_000;

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

    /// Per-process log ring buffers.
    log_buffers: HashMap<String, VecDeque<LogLine>>,

    /// Broadcast channel for log streaming.
    logs_tx: broadcast::Sender<LogLine>,

    /// Channel for receiving logs from PTY reader tasks.
    log_rx: mpsc::Receiver<LogLine>,

    /// Sender for PTY reader tasks to submit logs.
    log_tx: mpsc::Sender<LogLine>,
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
}

impl Engine {
    /// Create a new engine from a configuration file.
    pub fn new(config_path: &Path) -> Result<Self, DaemonError> {
        let config = zaz_config::load(config_path).map_err(DaemonError::Config)?;
        Self::from_config(config, config_path.to_path_buf())
    }

    /// Create a new engine from a loaded configuration.
    pub fn from_config(config: Config, config_path: PathBuf) -> Result<Self, DaemonError> {
        // Determine the config directory for variable expansion
        let config_dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        // Create watcher configuration
        let watcher_config = WatcherConfig {
            root: config_dir.clone(),
            debounce: Duration::from_millis(config.settings.debounce_ms),
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

            // Create executor with shell and working directory
            let mut executor = Executor::new(config.settings.shell.clone());
            if let Some(ref dir) = group.working_dir {
                executor = executor.with_working_dir(dir.clone());
            }

            // Create daemons
            let daemons: Vec<Daemon> = group
                .daemons
                .iter()
                .map(|d| Daemon::new(d.clone(), executor.clone()))
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

            groups.insert(
                group.name.clone(),
                ManagedGroup {
                    config: group.clone(),
                    executor,
                    daemons,
                    state,
                },
            );
        }

        // Compute execution order based on dependencies
        let execution_order = topological_sort(&config.groups)?;

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

        // Create broadcast channel for log streaming (larger capacity for high-volume output)
        let (logs_tx, _) = broadcast::channel(1024);

        // Create mpsc channel for PTY reader tasks to submit logs
        let (log_tx, log_rx) = mpsc::channel(1024);

        Ok(Self {
            config,
            config_path,
            watcher,
            group_patterns,
            groups,
            state,
            execution_order,
            status_tx,
            log_buffers: HashMap::new(),
            logs_tx,
            log_rx,
            log_tx,
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
        self.logs_tx.subscribe()
    }

    /// Get a sender for submitting logs (for the tracing layer).
    pub fn log_sender(&self) -> mpsc::Sender<LogLine> {
        self.log_tx.clone()
    }

    /// Add a log line to storage and broadcast.
    pub fn push_log(&mut self, log: LogLine) {
        eprintln!(
            "[DEBUG push_log] process={} source={:?} content={}",
            log.process,
            log.source,
            log.content.chars().take(60).collect::<String>()
        );

        // Store in per-process buffer
        let buffer = self
            .log_buffers
            .entry(log.process.clone())
            .or_insert_with(VecDeque::new);
        buffer.push_back(log.clone());

        // Trim to max size
        while buffer.len() > MAX_LOG_LINES {
            buffer.pop_front();
        }

        // Broadcast to subscribers (ignore errors if no subscribers)
        let _ = self.logs_tx.send(log);
    }

    /// Get stored logs for a process.
    ///
    /// If `name` is "*", returns logs from all processes.
    pub fn get_logs(&self, name: &str, limit: Option<usize>) -> Vec<LogLine> {
        let total_in_buffers: usize = self.log_buffers.values().map(|b| b.len()).sum();
        eprintln!(
            "[DEBUG get_logs] name={} limit={:?} total_in_buffers={}",
            name, limit, total_in_buffers
        );

        if name == "*" {
            // Return all logs, sorted by timestamp
            let mut all: Vec<LogLine> = self
                .log_buffers
                .values()
                .flat_map(|buf| buf.iter().cloned())
                .collect();
            all.sort_by_key(|l| l.timestamp);
            let result = if let Some(n) = limit {
                all.into_iter()
                    .rev()
                    .take(n)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            } else {
                all
            };
            eprintln!("[DEBUG get_logs] returning {} logs", result.len());
            result
        } else {
            self.log_buffers
                .get(name)
                .map(|buf| {
                    let iter = buf.iter().cloned();
                    match limit {
                        Some(n) => iter
                            .rev()
                            .take(n)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect(),
                        None => iter.collect(),
                    }
                })
                .unwrap_or_default()
        }
    }

    /// Spawn a background task to read PTY output and push to logs.
    fn spawn_pty_reader(
        &self,
        process: String,
        group: Option<String>,
        reader: Box<dyn std::io::Read + Send>,
    ) {
        use std::io::BufRead;

        let log_tx = self.log_tx.clone();

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

    /// Process incoming logs from PTY readers.
    ///
    /// This should be called regularly from the main loop to drain the log channel.
    pub fn process_incoming_logs(&mut self) {
        while let Ok(log) = self.log_rx.try_recv() {
            self.push_log(log);
        }
    }

    /// Run the initial startup sequence.
    ///
    /// This runs all tasks (respecting on_change_only) and starts all daemons.
    pub async fn startup(&mut self) -> Result<(), DaemonError> {
        tracing::info!("starting initial run");
        self.state.status = DaemonStatus::Running;

        // Run groups in dependency order
        for group_name in &self.execution_order.clone() {
            self.run_group(group_name, &[], false).await?;
        }

        Ok(())
    }

    /// Run a single group's tasks and start its daemons.
    async fn run_group(
        &mut self,
        group_name: &str,
        changed_files: &[PathBuf],
        is_change_triggered: bool,
    ) -> Result<(), DaemonError> {
        // First, extract what we need from the group to avoid borrow issues
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

            // Create unbounded channel for streaming output (bounded can drop lines)
            let (output_tx, mut output_rx) = mpsc::unbounded_channel::<OutputLine>();

            // Clone values needed for the spawned task
            let task_runner_clone = task_runner.clone();
            let command_clone = command.clone();

            // Spawn task execution in background
            let task_handle = tokio::spawn(async move {
                task_runner_clone
                    .run_streaming(&command_clone, output_tx)
                    .await
            });

            // Receive and push output lines as they arrive
            let task_name = task.name().to_string();
            let group_name_owned = group_name.to_string();
            while let Some(line) = output_rx.recv().await {
                let content = match line {
                    OutputLine::Stdout(s) => s,
                    OutputLine::Stderr(s) => s,
                };
                self.push_log(
                    LogLine::process(&task_name, content).with_group(group_name_owned.clone()),
                );
            }

            // Wait for task to complete - handle JoinError from spawn
            let inner_result = task_handle.await.map_err(|e| DaemonError::TaskFailed {
                task: task_name.clone(),
                error: format!("task panicked: {}", e),
            })?;

            match inner_result {
                Ok(output) => {
                    let duration = start.elapsed();

                    let is_success = output.exit_code.map(|c| c == 0).unwrap_or(true);
                    if is_success {
                        // Push daemon log for task completion
                        self.push_log(
                            LogLine::daemon(
                                &task_name,
                                format!("completed in {}ms", duration.as_millis()),
                            )
                            .with_group(group_name_owned.clone()),
                        );

                        tracing::info!(
                            task = %task_name,
                            duration_ms = duration.as_millis(),
                            exit_code = output.exit_code,
                            "task completed"
                        );
                        if let Some(group) = self.groups.get_mut(group_name) {
                            group.state.tasks[idx].status = ProcessStatus::Success;
                            group.state.tasks[idx].duration_ms = Some(duration.as_millis() as u64);
                            group.state.tasks[idx].exit_code = output.exit_code;
                        }
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
    pub async fn handle_changes(&mut self, events: Vec<FileEvent>) -> Result<(), DaemonError> {
        if events.is_empty() {
            return Ok(());
        }

        let changed_paths: Vec<PathBuf> = events.iter().map(|e| e.path.clone()).collect();
        tracing::info!(files = changed_paths.len(), "processing file changes");

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

        // Sort affected groups by execution order
        let order: HashMap<&str, usize> = self
            .execution_order
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        affected_groups.sort_by_key(|n| order.get(n.as_str()).copied().unwrap_or(usize::MAX));

        // Run affected groups with their dependencies
        for group_name in affected_groups {
            self.run_group(&group_name, &changed_paths, true).await?;
        }

        Ok(())
    }

    /// Poll for file changes and process them.
    pub async fn poll(&mut self) -> Result<bool, DaemonError> {
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
    pub async fn check_daemons(&mut self) -> Result<(), DaemonError> {
        // Collect PTY readers for restarted daemons
        let mut pty_readers: Vec<(String, Option<String>, Box<dyn std::io::Read + Send>)> =
            Vec::new();

        for (group_name, group) in self.groups.iter_mut() {
            for (idx, daemon) in group.daemons.iter_mut().enumerate() {
                let exited = daemon.check().await.map_err(DaemonError::Process)?;

                if exited {
                    // Daemon exited, update state
                    group.state.daemons[idx].status = ProcessStatus::Backoff;
                    group.state.daemons[idx].pid = None;

                    // Auto-restart after delay
                    let delay = daemon.restart_delay();
                    tracing::info!(
                        daemon = %daemon.name(),
                        delay_ms = delay.as_millis(),
                        "daemon exited, will restart"
                    );

                    tokio::time::sleep(delay).await;
                    daemon.start().map_err(DaemonError::Process)?;

                    // Get PTY reader for streaming output
                    if let Some(reader) = daemon.try_clone_reader() {
                        pty_readers.push((
                            daemon.name().to_string(),
                            Some(group_name.clone()),
                            reader,
                        ));
                    }

                    group.state.daemons[idx].status = ProcessStatus::Running;
                    group.state.daemons[idx].pid = daemon.pid();
                }
            }
        }

        // Spawn PTY reader tasks (outside the mutable borrow)
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
    pub async fn restart_group(&mut self, group_name: &str) -> Result<(), DaemonError> {
        tracing::info!(group = group_name, "restarting group");
        self.run_group(group_name, &[], false).await
    }

    /// Restart all groups.
    pub async fn restart_all(&mut self) -> Result<(), DaemonError> {
        tracing::info!("restarting all groups");
        for group_name in &self.execution_order.clone() {
            self.run_group(group_name, &[], false).await?;
        }
        Ok(())
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
            ApiRequest::RestartGroup { name } => match self.restart_group(&name).await {
                Ok(()) => ApiResponse::ok_with_message(format!("restarted group '{}'", name)),
                Err(e) => ApiResponse::error(format!("failed to restart group '{}': {}", name, e)),
            },
            ApiRequest::RestartAll => match self.restart_all().await {
                Ok(()) => ApiResponse::ok_with_message("restarted all groups"),
                Err(e) => ApiResponse::error(format!("failed to restart: {}", e)),
            },
            ApiRequest::ReloadConfig => {
                // TODO: implement config hot-reload
                ApiResponse::error("config reload not yet implemented")
            }
            ApiRequest::Shutdown => {
                // Signal handled by caller
                ApiResponse::ok_with_message("shutting down")
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
    use zaz_config::Group;

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
}
