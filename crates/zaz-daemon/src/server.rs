//! Unix socket server.

use crate::{ApiRequest, ApiResponse, DaemonError, DaemonState};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

/// Unix socket server for the daemon API.
pub struct Server {
    listener: UnixListener,
    shutdown_tx: broadcast::Sender<()>,
}

impl Server {
    /// Create a new server bound to the given socket path.
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self, DaemonError> {
        let path = path.as_ref();

        // Remove existing socket file if it exists
        if path.exists() {
            std::fs::remove_file(path)?;
        }

        let listener = UnixListener::bind(path)?;
        let (shutdown_tx, _) = broadcast::channel(1);

        tracing::info!(path = %path.display(), "API server listening");

        Ok(Self {
            listener,
            shutdown_tx,
        })
    }

    /// Get a shutdown receiver.
    pub fn shutdown_receiver(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Trigger shutdown.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Accept and handle connections.
    pub async fn run(&self, state: DaemonState) -> Result<(), DaemonError> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let state = state.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, state).await {
                    tracing::error!(error = %e, "connection handler error");
                }
            });
        }
    }
}

async fn handle_connection(stream: UnixStream, state: DaemonState) -> Result<(), DaemonError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let request: ApiRequest = serde_json::from_str(&line)?;
        let response = handle_request(request, &state);

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;

        line.clear();
    }

    Ok(())
}

fn handle_request(request: ApiRequest, state: &DaemonState) -> ApiResponse {
    match request {
        ApiRequest::Status => ApiResponse::Status {
            state: state.clone(),
        },
        ApiRequest::ListGroups => ApiResponse::Status {
            state: state.clone(),
        },
        ApiRequest::GetLogs { name, lines: _ } => ApiResponse::Logs {
            name,
            lines: vec![], // TODO: implement log storage
        },
        ApiRequest::RestartGroup { name } => {
            tracing::info!(group = %name, "restart requested");
            ApiResponse::ok_with_message(format!("restarting group: {}", name))
        }
        ApiRequest::RestartAll => {
            tracing::info!("restart all requested");
            ApiResponse::ok_with_message("restarting all groups")
        }
        ApiRequest::ReloadConfig => {
            tracing::info!("config reload requested");
            ApiResponse::ok_with_message("reloading configuration")
        }
        ApiRequest::Shutdown => {
            tracing::info!("shutdown requested");
            ApiResponse::ok_with_message("shutting down")
        }
    }
}
