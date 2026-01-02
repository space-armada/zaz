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
}

/// A log line received from the daemon.
#[derive(Debug, Clone)]
pub struct LogLine {
    /// Process name that emitted this log.
    pub process: String,
    /// The log content.
    pub content: String,
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
    /// Whether we're currently connected.
    connected: Arc<AtomicBool>,
    /// Socket path for reconnection.
    socket_path: PathBuf,
    /// Handle to the background task.
    _task_handle: tokio::task::JoinHandle<()>,
}

impl DaemonConnection {
    /// Connect to the daemon at the given socket path.
    ///
    /// Spawns a background task that handles async communication.
    pub async fn connect(socket_path: &Path) -> Result<Self, TuiError> {
        let (command_tx, command_rx) = mpsc::channel::<ClientCommand>(32);
        let (state_tx, state_rx) = mpsc::channel::<DaemonState>(32);
        let (logs_tx, logs_rx) = mpsc::channel::<LogLine>(256);
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
                connected_clone,
            )
            .await;
        });

        Ok(Self {
            command_tx,
            state_rx,
            logs_rx,
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
}

/// Background task that handles async daemon communication.
async fn background_task(
    socket_path: PathBuf,
    mut command_rx: mpsc::Receiver<ClientCommand>,
    state_tx: mpsc::Sender<DaemonState>,
    _logs_tx: mpsc::Sender<LogLine>,
    connected: Arc<AtomicBool>,
) {
    let mut client: Option<Client> = None;
    let mut reconnect_delay = std::time::Duration::from_millis(100);
    const MAX_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

    loop {
        // Try to connect if not connected
        if client.is_none() {
            match Client::connect(&socket_path).await {
                Ok(c) => {
                    tracing::debug!("connected to daemon");
                    client = Some(c);
                    connected.store(true, Ordering::Relaxed);
                    reconnect_delay = std::time::Duration::from_millis(100);

                    // Request initial status
                    if let Some(ref mut c) = client {
                        if let Ok(ApiResponse::Status { state }) =
                            c.request(&ApiRequest::Status).await
                        {
                            let _ = state_tx.send(state).await;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "failed to connect to daemon, retrying...");
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
                    let result = handle_command(c, cmd, &state_tx).await;
                    if result.is_err() {
                        // Connection lost, will reconnect
                        tracing::debug!("connection lost, will reconnect");
                        client = None;
                        connected.store(false, Ordering::Relaxed);
                    }
                }
            }

            // Periodic status poll (every 500ms)
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if let Some(ref mut c) = client {
                    match c.request(&ApiRequest::Status).await {
                        Ok(ApiResponse::Status { state }) => {
                            let _ = state_tx.send(state).await;
                        }
                        Ok(_) => {}
                        Err(_) => {
                            // Connection lost
                            tracing::debug!("connection lost during poll, will reconnect");
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
) -> Result<(), TuiError> {
    let request = match cmd {
        ClientCommand::RestartGroup(name) => ApiRequest::RestartGroup { name },
        ClientCommand::RestartAll => ApiRequest::RestartAll,
        ClientCommand::Shutdown => ApiRequest::Shutdown,
        ClientCommand::RefreshStatus => ApiRequest::Status,
        ClientCommand::SubscribeLogs(name) => ApiRequest::SubscribeLogs { name },
        ClientCommand::UnsubscribeLogs(_) => {
            // TODO: implement unsubscribe
            return Ok(());
        }
    };

    let response = client
        .request(&request)
        .await
        .map_err(|e| TuiError::Connection(e.to_string()))?;

    // Handle response
    match response {
        ApiResponse::Status { state } => {
            let _ = state_tx.send(state).await;
        }
        ApiResponse::Ok { message } => {
            if let Some(msg) = message {
                tracing::info!("{}", msg);
            }
            // Refresh status after successful command
            if let Ok(ApiResponse::Status { state }) = client.request(&ApiRequest::Status).await {
                let _ = state_tx.send(state).await;
            }
        }
        ApiResponse::Error { message } => {
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
            process: "server".to_string(),
            content: "Started on :8080".to_string(),
        };
        assert_eq!(log.process, "server");
        assert_eq!(log.content, "Started on :8080");
    }
}
