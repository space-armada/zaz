//! Unix socket server.

use crate::{ApiRequest, ApiResponse, DaemonError, EngineCommand};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// Get socket path for a specific config file.
///
/// 1. If `.zaz/` directory exists in the config's parent directory, uses `.zaz/daemon.sock`
/// 2. Otherwise, uses `~/.local/state/zaz/<hash>.sock` where hash is based on config path
pub fn socket_path_for_config(config_path: &Path) -> PathBuf {
    // Canonicalize the config path for consistent hashing
    let canonical = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());

    // Check if .zaz directory exists in project
    if let Some(parent) = canonical.parent() {
        let zaz_dir = parent.join(".zaz");
        if zaz_dir.is_dir() {
            return zaz_dir.join("daemon.sock");
        }
    }

    // Fall back to user state directory with hashed path
    let hash = {
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        hasher.finish()
    };

    let state_dir = if let Ok(home) = std::env::var("HOME") {
        let dir = PathBuf::from(home).join(".local/state/zaz");
        if !dir.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        dir
    } else {
        // Last resort: /tmp with username
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        PathBuf::from(format!("/tmp/zaz-{}", user))
    };

    state_dir.join(format!("{:016x}.sock", hash))
}

/// Resolve the daemon socket for a command invocation.
///
/// Resolution order:
/// 1. Explicit `--socket` override, if provided
/// 2. Project config discovered by walking upward from `start_dir`
/// 3. Actionable error if no project config can be found
pub fn resolve_socket(
    explicit_socket: Option<PathBuf>,
    start_dir: &Path,
) -> Result<PathBuf, DaemonError> {
    if let Some(socket) = explicit_socket {
        return Ok(socket);
    }

    let config_path =
        discover_config_upward(start_dir).ok_or_else(|| DaemonError::SocketResolution {
            start_dir: start_dir.to_path_buf(),
        })?;

    Ok(socket_path_for_config(&config_path))
}

/// Walk upward from `start_dir` looking for a zaz config file.
///
/// Returns the path of the first matching `zaz.toml` or `zaz.json` found, or
/// `None` if the walk reaches the filesystem root without finding one.
pub fn discover_config_upward(start_dir: &Path) -> Option<PathBuf> {
    let mut current = if start_dir.is_file() {
        start_dir.parent()?.to_path_buf()
    } else {
        start_dir.to_path_buf()
    };

    loop {
        for filename in zaz_config::CONFIG_FILES {
            let path = current.join(filename);
            if path.is_file() {
                return Some(path);
            }
        }

        if !current.pop() {
            return None;
        }
    }
}

/// Default socket path (legacy, for when no config is known).
///
/// Uses `$XDG_RUNTIME_DIR/zaz.sock` if available (preferred).
/// Falls back to `~/.local/state/zaz/zaz.sock` for user-specific access.
/// As a last resort, uses `/tmp/zaz-<username>.sock`.
#[cfg_attr(not(test), allow(dead_code))]
fn default_socket_path() -> PathBuf {
    // Prefer XDG_RUNTIME_DIR (e.g., /run/user/1000/) - already user-private
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("zaz.sock");
    }

    // Fall back to user's state directory (user-private via home directory permissions)
    if let Ok(home) = std::env::var("HOME") {
        let state_dir = PathBuf::from(home).join(".local/state/zaz");
        if !state_dir.exists() && std::fs::create_dir_all(&state_dir).is_ok() {
            let _ = std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700));
        }
        return state_dir.join("zaz.sock");
    }

    // Last resort: /tmp with username to avoid conflicts (less secure)
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/zaz-{}.sock", user))
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
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

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
                    tracing::info!("client connected");
                    let command_tx = self.command_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, command_tx).await {
                            tracing::info!(error = %e, "client disconnected");
                        } else {
                            tracing::info!("client disconnected");
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
    use tempfile::TempDir;

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

    #[test]
    fn test_resolve_socket_explicit_socket_wins() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "").unwrap();

        let explicit = temp.path().join("custom.sock");
        let resolved = resolve_socket(Some(explicit.clone()), temp.path()).unwrap();

        assert_eq!(resolved, explicit);
    }

    #[test]
    fn test_resolve_socket_uses_local_toml_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "").unwrap();

        let resolved = resolve_socket(None, temp.path()).unwrap();

        assert_eq!(resolved, socket_path_for_config(&config_path));
    }

    #[test]
    fn test_resolve_socket_walks_upward_to_parent_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "").unwrap();

        let nested = temp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        let resolved = resolve_socket(None, &nested).unwrap();

        assert_eq!(resolved, socket_path_for_config(&config_path));
    }

    #[test]
    fn test_resolve_socket_prefers_toml_before_json() {
        let temp = TempDir::new().unwrap();
        let toml_path = temp.path().join("zaz.toml");
        let json_path = temp.path().join("zaz.json");
        std::fs::write(&toml_path, "").unwrap();
        std::fs::write(&json_path, "{}").unwrap();

        let resolved = resolve_socket(None, temp.path()).unwrap();

        assert_eq!(resolved, socket_path_for_config(&toml_path));
    }

    #[test]
    fn test_resolve_socket_uses_json_when_toml_absent() {
        let temp = TempDir::new().unwrap();
        let json_path = temp.path().join("zaz.json");
        std::fs::write(&json_path, "{}").unwrap();

        let resolved = resolve_socket(None, temp.path()).unwrap();

        assert_eq!(resolved, socket_path_for_config(&json_path));
    }

    #[test]
    fn test_resolve_socket_errors_when_no_project_config_found() {
        let temp = TempDir::new().unwrap();
        let nested = temp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        let err = resolve_socket(None, &nested).unwrap_err();

        match &err {
            DaemonError::SocketResolution { start_dir } => assert_eq!(start_dir, &nested),
            other => panic!("expected SocketResolution error, got {:?}", other),
        }

        let message = err.to_string();
        assert!(message.contains("could not resolve daemon socket from"));
        assert_eq!(
            err.hint(),
            Some("run this command from a zaz project directory or pass --socket <PATH>")
        );
    }
}
