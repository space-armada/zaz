//! Daemon client wrapper used by MCP tools.
//!
//! Each call resolves the daemon socket from `cwd`, opens a fresh connection,
//! sends one request, and returns the typed response. Errors are mapped to
//! actionable [`McpError`] variants so tool handlers can surface useful
//! messages to the agent (e.g., "daemon is not running, start it with `zaz`").

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use zaz_config::Config;
use zaz_daemon::{
    discover_config_upward, resolve_socket, ApiRequest, ApiResponse, Client, DaemonState, LogLine,
};

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
pub async fn fetch_status(cwd: &Path) -> Result<DaemonState, McpError> {
    let mut client = open_client(cwd).await?;
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
pub async fn fetch_logs(cwd: &Path, req: &LogsRequest) -> Result<LogsPage, McpError> {
    let name = req.name.clone().unwrap_or_else(|| "*".to_string());
    let mut client = open_client(cwd).await?;
    let response = client
        .request(&ApiRequest::GetLogs {
            name: name.clone(),
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

/// Restart a single group by name.
pub async fn restart_group(cwd: &Path, name: &str) -> Result<String, McpError> {
    send_mutation(
        cwd,
        ApiRequest::RestartGroup {
            name: name.to_string(),
        },
        "restart_group",
    )
    .await
}

/// Restart a single process within a group.
pub async fn restart_process(
    cwd: &Path,
    group: &str,
    process: &str,
) -> Result<String, McpError> {
    send_mutation(
        cwd,
        ApiRequest::RestartProcess {
            group: group.to_string(),
            process: process.to_string(),
        },
        "restart_process",
    )
    .await
}

/// Restart every configured group.
pub async fn restart_all(cwd: &Path) -> Result<String, McpError> {
    send_mutation(cwd, ApiRequest::RestartAll, "restart_all").await
}

/// Reload the project configuration from disk.
pub async fn reload_config(cwd: &Path) -> Result<String, McpError> {
    send_mutation(cwd, ApiRequest::ReloadConfig, "reload_config").await
}

async fn send_mutation(
    cwd: &Path,
    request: ApiRequest,
    operation: &'static str,
) -> Result<String, McpError> {
    let mut client = open_client(cwd).await?;
    match client.request(&request).await? {
        ApiResponse::Ok { message } => Ok(message.unwrap_or_else(|| "ok".to_string())),
        ApiResponse::Error { message } => Err(McpError::DaemonRefused { operation, message }),
        other => Err(McpError::UnexpectedResponse(format!(
            "expected Ok for {operation}, got {other:?}"
        ))),
    }
}

/// Discover and load the project config file by walking upward from `cwd`.
pub fn discover_config(cwd: &Path) -> Result<(PathBuf, Config), McpError> {
    let path = discover_config_upward(cwd).ok_or_else(|| McpError::NoConfig {
        start_dir: cwd.to_path_buf(),
    })?;
    let config = zaz_config::load(&path)?;
    Ok((path, config))
}

async fn open_client(cwd: &Path) -> Result<Client, McpError> {
    let socket = resolve_socket(None, cwd)?;
    match Client::connect(&socket).await {
        Ok(client) => Ok(client),
        Err(zaz_daemon::DaemonError::Bind(io_err))
            if matches!(
                io_err.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Err(McpError::DaemonNotRunning { socket })
        }
        Err(other) => Err(McpError::from(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn fetch_status_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("zaz.toml"), "[settings]\n").unwrap();

        let err = fetch_status(tmp.path()).await.unwrap_err();
        match err {
            McpError::DaemonNotRunning { socket } => {
                assert!(
                    socket.starts_with(tmp.path()) || socket.is_absolute(),
                    "socket should be a real path, got {}",
                    socket.display()
                );
            }
            other => panic!("expected DaemonNotRunning, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn fetch_status_reports_no_config_outside_a_project() {
        let tmp = TempDir::new().unwrap();
        // No zaz.toml/zaz.json anywhere within tmp.
        let err = fetch_status(tmp.path()).await.unwrap_err();
        // resolve_socket maps the missing config to DaemonError::SocketResolution,
        // which surfaces as McpError::Daemon. The message should mention starting
        // from a project directory.
        let msg = err.to_string();
        assert!(
            msg.contains("could not resolve daemon socket") || msg.contains("no zaz config found"),
            "expected actionable message, got: {msg}"
        );
    }

    #[tokio::test]
    async fn restart_group_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("zaz.toml"), "[settings]\n").unwrap();
        let err = restart_group(tmp.path(), "backend").await.unwrap_err();
        assert!(
            matches!(err, McpError::DaemonNotRunning { .. }),
            "expected DaemonNotRunning, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn restart_process_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("zaz.toml"), "[settings]\n").unwrap();
        let err = restart_process(tmp.path(), "backend", "server")
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
        std::fs::write(tmp.path().join("zaz.toml"), "[settings]\n").unwrap();
        let err = restart_all(tmp.path()).await.unwrap_err();
        assert!(
            matches!(err, McpError::DaemonNotRunning { .. }),
            "expected DaemonNotRunning, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn reload_config_returns_daemon_not_running_when_socket_absent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("zaz.toml"), "[settings]\n").unwrap();
        let err = reload_config(tmp.path()).await.unwrap_err();
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

        let (path, _config) = discover_config(&nested).unwrap();
        assert_eq!(path, tmp.path().join("zaz.toml"));
    }

    #[test]
    fn discover_config_reports_missing_project() {
        let tmp = TempDir::new().unwrap();
        let err = discover_config(tmp.path()).unwrap_err();
        assert!(matches!(err, McpError::NoConfig { .. }));
    }
}
