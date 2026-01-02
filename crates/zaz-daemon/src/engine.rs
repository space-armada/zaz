//! Core orchestration engine for zaz.
//!
//! The engine ties together configuration, file watching, and process management.

use crate::state::{
    DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus,
};
use crate::{ApiResponse, DaemonError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::broadcast;
use zaz_config::{Config, Group};
use zaz_process::{Daemon, Executor, TaskRunner};
use zaz_vars::Context;
use zaz_watch::{FileEvent, PatternSet, Watcher, WatcherConfig};

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

    /// Managed groups with their state.
    groups: HashMap<String, ManagedGroup>,

    /// Current daemon state (for status queries).
    state: DaemonState,

    /// Topologically sorted group names for dependency ordering.
    execution_order: Vec<String>,

    /// Broadcast channel for status updates (for streaming subscribers).
    status_tx: broadcast::Sender<ApiResponse>,
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

        // Watch the config directory
        watcher.watch(&config_dir).map_err(DaemonError::Watch)?;

        // Build pattern sets and managed groups
        let mut group_patterns = HashMap::new();
        let mut groups = HashMap::new();

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

        Ok(Self {
            config,
            config_path,
            watcher,
            group_patterns,
            groups,
            state,
            execution_order,
            status_tx,
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
            match task_runner.run(&command).await {
                Ok(output) => {
                    let duration = start.elapsed();
                    tracing::info!(
                        task = %task.name(),
                        duration_ms = duration.as_millis(),
                        exit_code = output.exit_code,
                        "task completed"
                    );
                    if let Some(group) = self.groups.get_mut(group_name) {
                        group.state.tasks[idx].status = ProcessStatus::Success;
                        group.state.tasks[idx].duration_ms = Some(duration.as_millis() as u64);
                        group.state.tasks[idx].exit_code = output.exit_code;
                    }
                }
                Err(e) => {
                    tracing::error!(task = %task.name(), error = %e, "task failed");
                    if let Some(group) = self.groups.get_mut(group_name) {
                        group.state.tasks[idx].status = ProcessStatus::Failed;
                        group.state.status = GroupStatus::Failed;
                    }
                    self.update_state();
                    return Err(DaemonError::TaskFailed {
                        task: task.name().to_string(),
                        error: e.to_string(),
                    });
                }
            }
        }

        // Start or restart daemons
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
                }

                group.state.daemons[idx].status = ProcessStatus::Running;
                group.state.daemons[idx].pid = daemon.pid();
            }

            group.state.status = GroupStatus::Ready;
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
        for group in self.groups.values_mut() {
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

                    group.state.daemons[idx].status = ProcessStatus::Running;
                    group.state.daemons[idx].pid = daemon.pid();
                }
            }
        }

        self.update_state();
        Ok(())
    }

    /// Shutdown all processes gracefully.
    pub async fn shutdown(&mut self) -> Result<(), DaemonError> {
        tracing::info!("shutting down");
        self.state.status = DaemonStatus::Stopping;

        for group in self.groups.values_mut() {
            for daemon in &mut group.daemons {
                daemon.stop().map_err(DaemonError::Process)?;
            }
        }

        tracing::info!("waiting for daemons to exit");
        tokio::time::sleep(Duration::from_secs(2)).await;

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
            ApiRequest::GetLogs { name, lines: _ } => {
                // TODO: implement log storage
                ApiResponse::Logs {
                    name,
                    lines: vec![],
                }
            }
            ApiRequest::SubscribeLogs { name } => {
                // TODO: implement log streaming
                ApiResponse::Logs {
                    name,
                    lines: vec![],
                }
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
