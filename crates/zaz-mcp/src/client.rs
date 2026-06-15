//! Daemon client wrapper used by MCP tools.
//!
//! Each call opens a fresh connection to the daemon at the supplied
//! `socket_path` (resolved once by the bin), sends one request, and returns
//! the typed response. Errors are mapped to actionable [`McpError`] variants
//! so tool handlers can surface useful messages to the agent (e.g.,
//! "daemon is not running, start it with `zaz` or `zaz start`").

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use zaz_config::Config;
use zaz_daemon::{discover_config_upward, ApiRequest, ApiResponse, Client, DaemonState, LogLine};

use crate::error::McpError;
use crate::types::LogsRequest;

/// Result of a paginated logs query, mirroring the daemon's `ApiResponse::Logs` shape.
#[derive(Debug, Clone)]
pub struct LogsPage {
    pub name: String,
    pub lines: Vec<LogLine>,
    pub total_count: Option<usize>,
    pub has_more: Option<bool>,
    pub offset: Option<usize>,
}

/// Fetch the daemon's overall state.
pub async fn fetch_status(socket_path: &Path) -> Result<DaemonState, McpError> {
    let mut client = open_client(socket_path).await?;
    match client.request(&ApiRequest::Status).await? {
        ApiResponse::Status { state } => Ok(state),
        ApiResponse::Error { message } => Err(McpError::UnexpectedResponse(message)),
        other => Err(McpError::UnexpectedResponse(format!(
            "expected Status, got {:?}",
            other
        ))),
    }
}

/// Fetch a paginated page of logs.
pub async fn fetch_logs(socket_path: &Path, req: &LogsRequest) -> Result<LogsPage, McpError> {
    let name = req.name.clone().unwrap_or_else(|| "*".to_string());
    let mut client = open_client(socket_path).await?;
    let response = client
        .request(&ApiRequest::GetLogs {
            name: name.clone(),
            project: req.project.clone(),
            lines: None,
            offset: req.offset,
            limit: req.limit,
            search: req.search.clone(),
        })
        .await?;
    match response {
        ApiResponse::Logs {
            name,
            lines,
            total_count,
            has_more,
            offset,
        } => Ok(LogsPage {
            name,
            lines,
            total_count,
            has_more,
            offset,
        }),
        ApiResponse::Error { message } => Err(McpError::UnexpectedResponse(message)),
        other => Err(McpError::UnexpectedResponse(format!(
            "expected Logs, got {:?}",
            other
        ))),
    }
}

/// Restart a single group by name, optionally scoped to a workspace project.
pub async fn restart_group(
    socket_path: &Path,
    name: &str,
    project: Option<&str>,
) -> Result<String, McpError> {
    send_mutation(
        socket_path,
        ApiRequest::RestartGroup {
            name: name.to_string(),
            project: project.map(str::to_string),
        },
        "restart_group",
    )
    .await
}

/// Restart a single process within a group, optionally scoped to a project.
pub async fn restart_process(
    socket_path: &Path,
    group: &str,
    process: &str,
    project: Option<&str>,
) -> Result<String, McpError> {
    send_mutation(
        socket_path,
        ApiRequest::RestartProcess {
            group: group.to_string(),
            process: process.to_string(),
            project: project.map(str::to_string),
        },
        "restart_process",
    )
    .await
}

/// Restart every configured group.
pub async fn restart_all(socket_path: &Path) -> Result<String, McpError> {
    send_mutation(socket_path, ApiRequest::RestartAll, "restart_all").await
}

/// Reload the project configuration from disk.
pub async fn reload_config(socket_path: &Path) -> Result<String, McpError> {
    send_mutation(socket_path, ApiRequest::ReloadConfig, "reload_config").await
}

async fn send_mutation(
    socket_path: &Path,
    request: ApiRequest,
    operation: &'static str,
) -> Result<String, McpError> {
    let mut client = open_client(socket_path).await?;
    match client.request(&request).await? {
        ApiResponse::Ok { message } => Ok(message.unwrap_or_else(|| "ok".to_string())),
        ApiResponse::Error { message } => Err(McpError::DaemonRefused { operation, message }),
        other => Err(McpError::UnexpectedResponse(format!(
            "expected Ok for {operation}, got {other:?}"
        ))),
    }
}

/// Discover and load the project config file. An `explicit` path takes
/// precedence over the CWD walk-up; if neither yields a config, returns
/// [`McpError::NoConfig`].
pub fn discover_config(explicit: Option<&Path>, cwd: &Path) -> Result<(PathBuf, Config), McpError> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => discover_config_upward(cwd).ok_or_else(|| McpError::NoConfig {
            start_dir: cwd.to_path_buf(),
        })?,
    };
    let config = zaz_config::load(&path)?;
    Ok((path, config))
}

async fn open_client(socket_path: &Path) -> Result<Client, McpError> {
    match Client::connect(socket_path).await {
        Ok(client) => Ok(client),
        Err(zaz_daemon::DaemonError::Bind(io_err))
            if matches!(
                io_err.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Err(McpError::DaemonNotRunning {
                socket: socket_path.to_path_buf(),
            })
        }
        Err(other) => Err(McpError::from(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn missing_socket(dir: &Path) -> PathBuf {
        dir.join("nonexistent.sock")
    }

    #[tokio::test]
    async fn fetch_status_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        let socket = missing_socket(tmp.path());
        let err = fetch_status(&socket).await.unwrap_err();
        match err {
            McpError::DaemonNotRunning { socket: s } => {
                assert_eq!(s, socket, "error must echo the supplied socket path");
            }
            other => panic!("expected DaemonNotRunning, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn restart_group_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        let socket = missing_socket(tmp.path());
        let err = restart_group(&socket, "backend", None).await.unwrap_err();
        assert!(
            matches!(err, McpError::DaemonNotRunning { .. }),
            "expected DaemonNotRunning, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn restart_process_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        let socket = missing_socket(tmp.path());
        let err = restart_process(&socket, "backend", "server", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::DaemonNotRunning { .. }),
            "expected DaemonNotRunning, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn restart_all_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        let socket = missing_socket(tmp.path());
        let err = restart_all(&socket).await.unwrap_err();
        assert!(
            matches!(err, McpError::DaemonNotRunning { .. }),
            "expected DaemonNotRunning, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn reload_config_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        let socket = missing_socket(tmp.path());
        let err = reload_config(&socket).await.unwrap_err();
        assert!(
            matches!(err, McpError::DaemonNotRunning { .. }),
            "expected DaemonNotRunning, got: {err:?}"
        );
    }

    #[test]
    fn discover_config_walks_upward() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("zaz.toml"),
            "[settings]\ndebounce = \"100ms\"\n",
        )
        .unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let (path, _config) = discover_config(None, &nested).unwrap();
        assert_eq!(path, tmp.path().join("zaz.toml"));
    }

    #[test]
    fn discover_config_reports_missing_project() {
        let tmp = TempDir::new().unwrap();
        let err = discover_config(None, tmp.path()).unwrap_err();
        assert!(matches!(err, McpError::NoConfig { .. }));
    }

    #[test]
    fn discover_config_honors_explicit_path() {
        // Outer directory has its own zaz.toml that walk-up would otherwise pick.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("zaz.toml"), "[settings]\n").unwrap();

        // Explicit config sits in a sibling subdirectory.
        let custom_dir = tmp.path().join("custom");
        std::fs::create_dir_all(&custom_dir).unwrap();
        let custom_path = custom_dir.join("alt.toml");
        std::fs::write(&custom_path, "[settings]\ndebounce = \"250ms\"\n").unwrap();

        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let (path, _config) = discover_config(Some(&custom_path), &nested).unwrap();
        assert_eq!(path, custom_path);
    }

    #[tokio::test]
    async fn fetch_status_uses_provided_socket_path() {
        // A path that has no chance of being walked-up to from cwd; the error
        // must echo it back verbatim, proving the supplied path was used.
        let socket = PathBuf::from("/tmp/zaz-mcp-explicit-override-test.sock");
        let _ = std::fs::remove_file(&socket);
        let err = fetch_status(&socket).await.unwrap_err();
        match err {
            McpError::DaemonNotRunning { socket: s } => assert_eq!(s, socket),
            other => panic!("expected DaemonNotRunning, got: {:?}", other),
        }
    }
}
