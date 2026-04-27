//! Daemon connection wrapper for TUI.
//!
//! Provides non-blocking communication with the daemon via channels,
//! suitable for integration with the synchronous TUI event loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use zaz_daemon::{ApiRequest, ApiResponse, Client, DaemonState};

use crate::TuiError;

/// Commands that can be sent to the daemon.
#[derive(Debug, Clone)]
pub enum ClientCommand {
    /// Restart a specific group.
    RestartGroup(String),
    /// Restart a specific process (task or daemon) within a group.
    RestartProcess { group: String, process: String },
    /// Restart all groups.
    RestartAll,
    /// Shutdown the daemon.
    Shutdown,
    /// Subscribe to logs for a process.
    SubscribeLogs(String),
    /// Unsubscribe from logs for a process.
    UnsubscribeLogs(String),
    /// Request current status (one-shot).
    RefreshStatus,
    /// Fetch a page of historical logs.
    FetchPage {
        name: String,
        offset: usize,
        limit: usize,
    },
}

/// Result of a page fetch from the daemon.
#[derive(Debug)]
pub struct PageFetchResult {
    /// Process name (or "*" for combined).
    pub name: String,
    /// Offset of the first line in this result.
    pub offset: usize,
    /// The fetched log lines.
    pub lines: Vec<LogLine>,
    /// Total number of logs available in the daemon.
    pub total_count: usize,
}

/// Source of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// Output from a process (stdout/stderr).
    Process,
    /// Internal daemon log (zaz messages).
    Daemon,
}

/// A log line received from the daemon.
#[derive(Debug, Clone)]
pub struct LogLine {
    /// Timestamp in milliseconds since Unix epoch.
    pub timestamp: u64,
    /// Process name that emitted this log.
    pub process: String,
    /// Group name (optional).
    pub group: Option<String>,
    /// The log content.
    pub content: String,
    /// Source of the log.
    pub source: LogSource,
}

impl From<zaz_daemon::LogLine> for LogLine {
    fn from(line: zaz_daemon::LogLine) -> Self {
        Self {
            timestamp: line.timestamp,
            process: line.process,
            group: line.group,
            content: line.content,
            source: match line.source {
                zaz_daemon::LogSource::Process => LogSource::Process,
                zaz_daemon::LogSource::Daemon => LogSource::Daemon,
            },
        }
    }
}

/// Connection to the daemon with async background communication.
///
/// The TUI event loop can use `try_recv_*` methods to poll for updates
/// without blocking, while a background task handles the actual async I/O.
pub struct DaemonConnection {
    /// Send commands to the background task.
    command_tx: mpsc::Sender<ClientCommand>,
    /// Receive state updates from the background task.
    state_rx: mpsc::Receiver<DaemonState>,
    /// Receive log lines from the background task.
    logs_rx: mpsc::Receiver<LogLine>,
    /// Receive page fetch results from the background task.
    page_rx: mpsc::Receiver<PageFetchResult>,
    /// Whether we're currently connected.
    connected: Arc<AtomicBool>,
    /// Socket path for reconnection.
    socket_path: PathBuf,
    /// Handle to the background task.
    _task_handle: tokio::task::JoinHandle<()>,
}

fn command_name(cmd: &ClientCommand) -> &'static str {
    match cmd {
        ClientCommand::RestartGroup(_) => "RestartGroup",
        ClientCommand::RestartProcess { .. } => "RestartProcess",
        ClientCommand::RestartAll => "RestartAll",
        ClientCommand::Shutdown => "Shutdown",
        ClientCommand::SubscribeLogs(_) => "SubscribeLogs",
        ClientCommand::UnsubscribeLogs(_) => "UnsubscribeLogs",
        ClientCommand::RefreshStatus => "Status",
        ClientCommand::FetchPage { .. } => "GetLogs",
    }
}

impl DaemonConnection {
    /// Connect to the daemon at the given socket path.
    ///
    /// Spawns a background task that handles async communication.
    pub async fn connect(socket_path: &Path) -> Result<Self, TuiError> {
        tracing::debug!(
            socket = %socket_path.display(),
            "creating TUI daemon connection"
        );
        let (command_tx, command_rx) = mpsc::channel::<ClientCommand>(32);
        let (state_tx, state_rx) = mpsc::channel::<DaemonState>(32);
        let (logs_tx, logs_rx) = mpsc::channel::<LogLine>(256);
        let (page_tx, page_rx) = mpsc::channel::<PageFetchResult>(32);
        let connected = Arc::new(AtomicBool::new(false));

        let socket_path_owned = socket_path.to_path_buf();
        let connected_clone = connected.clone();

        // Spawn background task for daemon communication
        let task_handle = tokio::spawn(async move {
            background_task(
                socket_path_owned,
                command_rx,
                state_tx,
                logs_tx,
                page_tx,
                connected_clone,
            )
            .await;
        });

        Ok(Self {
            command_tx,
            state_rx,
            logs_rx,
            page_rx,
            connected,
            socket_path: socket_path.to_path_buf(),
            _task_handle: task_handle,
        })
    }

    /// Check if we're currently connected to the daemon.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Try to receive a state update without blocking.
    ///
    /// Returns `Some(state)` if an update is available, `None` otherwise.
    pub fn try_recv_state(&mut self) -> Option<DaemonState> {
        self.state_rx.try_recv().ok()
    }

    /// Try to receive a log line without blocking.
    ///
    /// Returns `Some(log)` if a log is available, `None` otherwise.
    pub fn try_recv_log(&mut self) -> Option<LogLine> {
        self.logs_rx.try_recv().ok()
    }

    /// Send a command to the daemon.
    ///
    /// Returns an error if the background task has stopped.
    pub fn send_command(&self, cmd: ClientCommand) -> Result<(), TuiError> {
        tracing::debug!(
            socket = %self.socket_path.display(),
            request = command_name(&cmd),
            "queueing daemon request from TUI"
        );
        self.command_tx
            .try_send(cmd)
            .map_err(|e| TuiError::Connection(format!("failed to send command: {}", e)))
    }

    /// Request a status refresh.
    pub fn refresh_status(&self) -> Result<(), TuiError> {
        self.send_command(ClientCommand::RefreshStatus)
    }

    /// Request restart of a specific group.
    pub fn restart_group(&self, name: &str) -> Result<(), TuiError> {
        self.send_command(ClientCommand::RestartGroup(name.to_string()))
    }

    /// Request restart of all groups.
    pub fn restart_all(&self) -> Result<(), TuiError> {
        self.send_command(ClientCommand::RestartAll)
    }

    /// Request daemon shutdown.
    pub fn shutdown(&self) -> Result<(), TuiError> {
        self.send_command(ClientCommand::Shutdown)
    }

    /// Try to receive a page fetch result without blocking.
    pub fn try_recv_page(&mut self) -> Option<PageFetchResult> {
        self.page_rx.try_recv().ok()
    }

    /// Request a page of historical logs from the daemon.
    pub fn fetch_page(&self, name: &str, offset: usize, limit: usize) -> Result<(), TuiError> {
        self.send_command(ClientCommand::FetchPage {
            name: name.to_string(),
            offset,
            limit,
        })
    }
}

/// Background task that handles async daemon communication.
async fn background_task(
    socket_path: PathBuf,
    mut command_rx: mpsc::Receiver<ClientCommand>,
    state_tx: mpsc::Sender<DaemonState>,
    logs_tx: mpsc::Sender<LogLine>,
    page_tx: mpsc::Sender<PageFetchResult>,
    connected: Arc<AtomicBool>,
) {
    let mut client: Option<Client> = None;
    let mut reconnect_delay = std::time::Duration::from_millis(100);
    const MAX_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

    // Track last log timestamp to avoid duplicates
    let mut last_log_timestamp: u64 = 0;

    loop {
        // Try to connect if not connected
        if client.is_none() {
            tracing::debug!(
                socket = %socket_path.display(),
                reconnect_delay_ms = reconnect_delay.as_millis(),
                "attempting to connect to daemon"
            );
            match Client::connect(&socket_path).await {
                Ok(c) => {
                    tracing::debug!(socket = %socket_path.display(), "connected to daemon");
                    client = Some(c);
                    connected.store(true, Ordering::Relaxed);
                    reconnect_delay = std::time::Duration::from_millis(100);

                    // Request initial status
                    if let Some(ref mut c) = client {
                        tracing::debug!(
                            socket = %socket_path.display(),
                            request = "Status",
                            "requesting initial daemon status after connect"
                        );
                        if let Ok(ApiResponse::Status { state }) =
                            c.request(&ApiRequest::Status).await
                        {
                            let _ = state_tx.send(state).await;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        socket = %socket_path.display(),
                        error = %e,
                        "failed to connect to daemon, retrying"
                    );
                    connected.store(false, Ordering::Relaxed);
                    tokio::time::sleep(reconnect_delay).await;
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                    continue;
                }
            }
        }

        // Handle commands or poll for status
        tokio::select! {
            // Handle incoming commands
            Some(cmd) = command_rx.recv() => {
                if let Some(ref mut c) = client {
                    let request = command_name(&cmd);
                    let result = handle_command(c, cmd, &state_tx, &page_tx, &socket_path).await;
                    if result.is_err() {
                        // Connection lost, will reconnect
                        tracing::debug!(
                            socket = %socket_path.display(),
                            request,
                            "connection lost while handling daemon request, will reconnect"
                        );
                        client = None;
                        connected.store(false, Ordering::Relaxed);
                    }
                }
            }

            // Periodic status and logs poll (every 500ms)
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if let Some(ref mut c) = client {
                    // Poll status
                    tracing::trace!(socket = %socket_path.display(), request = "Status", "polling daemon status");
                    match c.request(&ApiRequest::Status).await {
                        Ok(ApiResponse::Status { state }) => {
                            let _ = state_tx.send(state).await;
                        }
                        Ok(_) => {}
                        Err(_) => {
                            // Connection lost
                            tracing::debug!(
                                socket = %socket_path.display(),
                                request = "Status",
                                "connection lost during status poll, will reconnect"
                            );
                            client = None;
                            connected.store(false, Ordering::Relaxed);
                            continue;
                        }
                    }

                    // Poll logs (get all logs, filter by timestamp)
                    tracing::trace!(
                        socket = %socket_path.display(),
                        request = "GetLogs",
                        name = "*",
                        limit = 500,
                        "polling daemon logs"
                    );
                    match c.request(&ApiRequest::GetLogs {
                        name: "*".to_string(),
                        lines: None, // Deprecated, use limit instead
                        offset: None,
                        limit: Some(500),
                        search: None,
                    }).await {
                        Ok(ApiResponse::Logs { lines, total_count, .. }) => {
                            tracing::debug!(
                                socket = %socket_path.display(),
                                request = "GetLogs",
                                name = "*",
                                returned_lines = lines.len(),
                                total_count,
                                previous_timestamp = last_log_timestamp,
                                "received daemon log poll response"
                            );
                            // Find max timestamp first, then send all new logs
                            // This avoids dropping logs with the same timestamp
                            let mut max_ts = last_log_timestamp;
                            for line in &lines {
                                if line.timestamp > max_ts {
                                    max_ts = line.timestamp;
                                }
                            }
                            // Send all logs newer than what we've previously seen
                            for line in lines {
                                if line.timestamp > last_log_timestamp {
                                    let _ = logs_tx.send(line.into()).await;
                                }
                            }
                            // Update after processing all
                            last_log_timestamp = max_ts;

                            // Send total count update via page channel
                            if let Some(tc) = total_count {
                                let _ = page_tx.send(PageFetchResult {
                                    name: "*".to_string(),
                                    offset: 0,
                                    lines: vec![],
                                    total_count: tc,
                                }).await;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            // Connection lost
                            tracing::debug!(
                                socket = %socket_path.display(),
                                request = "GetLogs",
                                name = "*",
                                "connection lost during log poll, will reconnect"
                            );
                            client = None;
                            connected.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }
}

/// Handle a single command.
async fn handle_command(
    client: &mut Client,
    cmd: ClientCommand,
    state_tx: &mpsc::Sender<DaemonState>,
    page_tx: &mpsc::Sender<PageFetchResult>,
    socket_path: &Path,
) -> Result<(), TuiError> {
    // Handle FetchPage separately since it routes through page_tx
    if let ClientCommand::FetchPage {
        name,
        offset,
        limit,
    } = cmd
    {
        tracing::debug!(
            socket = %socket_path.display(),
            request = "GetLogs",
            name = %name,
            offset,
            limit,
            "requesting historical logs from daemon"
        );
        let response = client
            .request(&ApiRequest::GetLogs {
                name: name.clone(),
                lines: None,
                offset: Some(offset),
                limit: Some(limit),
                search: None,
            })
            .await
            .map_err(|e| TuiError::Connection(e.to_string()))?;

        if let ApiResponse::Logs {
            lines, total_count, ..
        } = response
        {
            tracing::debug!(
                socket = %socket_path.display(),
                request = "GetLogs",
                name = %name,
                offset,
                limit,
                returned_lines = lines.len(),
                total_count = total_count.unwrap_or(0),
                "received historical log page from daemon"
            );
            let _ = page_tx
                .send(PageFetchResult {
                    name,
                    offset,
                    lines: lines.into_iter().map(|l| l.into()).collect(),
                    total_count: total_count.unwrap_or(0),
                })
                .await;
        }

        return Ok(());
    }

    let request_name = command_name(&cmd);
    let request = match cmd {
        ClientCommand::RestartGroup(name) => ApiRequest::RestartGroup { name },
        ClientCommand::RestartProcess { group, process } => {
            ApiRequest::RestartProcess { group, process }
        }
        ClientCommand::RestartAll => ApiRequest::RestartAll,
        ClientCommand::Shutdown => ApiRequest::Shutdown,
        ClientCommand::RefreshStatus => ApiRequest::Status,
        ClientCommand::SubscribeLogs(name) => ApiRequest::SubscribeLogs { name },
        ClientCommand::UnsubscribeLogs(_) => {
            // TODO: implement unsubscribe
            return Ok(());
        }
        ClientCommand::FetchPage { .. } => unreachable!(),
    };

    tracing::debug!(
        socket = %socket_path.display(),
        request = request_name,
        "sending daemon request"
    );

    let response = client
        .request(&request)
        .await
        .map_err(|e| TuiError::Connection(e.to_string()))?;

    // Handle response
    match response {
        ApiResponse::Status { state } => {
            tracing::debug!(
                socket = %socket_path.display(),
                request = "Status",
                groups = state.groups.len(),
                status = ?state.status,
                "received daemon status response"
            );
            let _ = state_tx.send(state).await;
        }
        ApiResponse::Ok { message } => {
            tracing::debug!(
                socket = %socket_path.display(),
                request = request_name,
                message = message.as_deref(),
                "daemon request completed successfully"
            );
            if let Some(msg) = message {
                tracing::info!("{}", msg);
            }
            // Refresh status after successful command
            tracing::debug!(
                socket = %socket_path.display(),
                request = "Status",
                "refreshing daemon status after successful command"
            );
            if let Ok(ApiResponse::Status { state }) = client.request(&ApiRequest::Status).await {
                let _ = state_tx.send(state).await;
            }
        }
        ApiResponse::Error { message } => {
            tracing::debug!(
                socket = %socket_path.display(),
                request = request_name,
                error = %message,
                "daemon request returned error response"
            );
            tracing::error!("daemon error: {}", message);
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_command_variants() {
        // Just ensure the enum variants compile
        let _ = ClientCommand::RestartGroup("test".to_string());
        let _ = ClientCommand::RestartAll;
        let _ = ClientCommand::Shutdown;
        let _ = ClientCommand::RefreshStatus;
    }

    #[test]
    fn test_log_line() {
        let log = LogLine {
            timestamp: 1234567890,
            process: "server".to_string(),
            group: Some("web".to_string()),
            content: "Started on :8080".to_string(),
            source: LogSource::Process,
        };
        assert_eq!(log.process, "server");
        assert_eq!(log.content, "Started on :8080");
        assert_eq!(log.source, LogSource::Process);
    }

    #[test]
    fn test_command_name_labels_requests() {
        assert_eq!(command_name(&ClientCommand::RefreshStatus), "Status");
        assert_eq!(command_name(&ClientCommand::Shutdown), "Shutdown");
        assert_eq!(
            command_name(&ClientCommand::FetchPage {
                name: "*".to_string(),
                offset: 0,
                limit: 10,
            }),
            "GetLogs"
        );
    }
}
