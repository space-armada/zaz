//! Unix socket server.

use crate::{ApiRequest, ApiResponse, DaemonError, EngineCommand};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// Default socket path.
pub fn default_socket_path() -> PathBuf {
    // Use XDG_RUNTIME_DIR if available, otherwise /tmp
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("zaz.sock")
    } else {
        PathBuf::from("/tmp").join(format!("zaz-{}.sock", std::process::id()))
    }
}

/// Unix socket server for the daemon API.
pub struct Server {
    listener: UnixListener,
    socket_path: PathBuf,
    command_tx: mpsc::Sender<EngineCommand>,
}

impl Server {
    /// Create a new server bound to the given socket path.
    pub async fn bind(
        path: impl AsRef<Path>,
        command_tx: mpsc::Sender<EngineCommand>,
    ) -> Result<Self, DaemonError> {
        let path = path.as_ref();

        // Remove existing socket file if it exists
        if path.exists() {
            std::fs::remove_file(path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(path)?;

        tracing::info!(path = %path.display(), "API server listening");

        Ok(Self {
            listener,
            socket_path: path.to_path_buf(),
            command_tx,
        })
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Accept and handle connections.
    pub async fn run(&self) -> Result<(), DaemonError> {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let command_tx = self.command_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, command_tx).await {
                            tracing::debug!(error = %e, "connection closed");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "accept error");
                }
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Clean up socket file
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn handle_connection(
    stream: UnixStream,
    command_tx: mpsc::Sender<EngineCommand>,
) -> Result<(), DaemonError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let response = match serde_json::from_str::<ApiRequest>(&line) {
            Ok(request) => {
                // Send command to engine and wait for response
                let (response_tx, response_rx) = tokio::sync::oneshot::channel();
                let command = EngineCommand {
                    request,
                    response_tx,
                };

                if command_tx.send(command).await.is_err() {
                    ApiResponse::error("engine unavailable")
                } else {
                    match response_rx.await {
                        Ok(response) => response,
                        Err(_) => ApiResponse::error("no response from engine"),
                    }
                }
            }
            Err(e) => ApiResponse::error(format!("invalid request: {}", e)),
        };

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;

        line.clear();
    }

    Ok(())
}

/// Client for connecting to the daemon API.
pub struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    /// Connect to the daemon at the given socket path.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, DaemonError> {
        let stream = UnixStream::connect(path.as_ref()).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    /// Send a request and receive a response.
    pub async fn request(&mut self, request: &ApiRequest) -> Result<ApiResponse, DaemonError> {
        let request_json = serde_json::to_string(request)?;
        self.writer.write_all(request_json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;

        let mut line = String::new();
        self.reader.read_line(&mut line).await?;

        let response: ApiResponse = serde_json::from_str(&line)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiRequest;
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "requires Unix socket permissions"]
    async fn test_client_server_communication() {
        // This test requires Unix socket creation which may be blocked by sandbox
        let test_dir = std::path::PathBuf::from("/tmp/claude/zaz-test");
        std::fs::create_dir_all(&test_dir).unwrap();
        let socket_path = test_dir.join(format!("test-{}.sock", std::process::id()));

        let (command_tx, mut command_rx) = mpsc::channel::<EngineCommand>(32);

        // Start server
        let server = Server::bind(&socket_path, command_tx).await.unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = server.run().await;
        });

        // Give the server time to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Start a handler to respond to requests
        let handler = tokio::spawn(async move {
            if let Some(cmd) = command_rx.recv().await {
                let response = match cmd.request {
                    ApiRequest::Status => ApiResponse::ok_with_message("test status"),
                    _ => ApiResponse::error("unexpected request"),
                };
                let _ = cmd.response_tx.send(response);
            }
        });

        // Connect client and send request
        let mut client = Client::connect(&socket_path).await.unwrap();
        let response = client.request(&ApiRequest::Status).await.unwrap();

        match response {
            ApiResponse::Ok { message } => {
                assert_eq!(message.as_deref(), Some("test status"));
            }
            _ => panic!("expected Ok response, got {:?}", response),
        }

        // Cleanup
        handler.abort();
        server_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_default_socket_path() {
        let path = default_socket_path();
        // Should return a valid path ending in zaz.sock or zaz-<pid>.sock
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(
            filename.starts_with("zaz"),
            "socket filename should start with 'zaz'"
        );
    }
}
