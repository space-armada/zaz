//! API request and response types.

use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Source of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogSource {
    /// Output from a process (stdout/stderr).
    Process,
    /// Internal daemon log (zaz messages).
    Daemon,
}

/// Kind of output stream for process logs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    /// Standard output.
    #[default]
    Stdout,
    /// Standard error.
    Stderr,
    /// Combined output (from PTY, cannot distinguish).
    Combined,
}

/// A single log line with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    /// Timestamp in milliseconds since Unix epoch.
    pub timestamp: u64,
    /// Process/task name this log is associated with.
    pub process: String,
    /// Group name (optional, for context).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The log content.
    pub content: String,
    /// Source of the log (process output vs daemon internal).
    pub source: LogSource,
    /// Kind of output stream (stdout, stderr, or combined).
    /// Only meaningful when source is Process.
    #[serde(default)]
    pub output_kind: OutputKind,
}

impl LogLine {
    /// Create a new process log line, defaulting to combined output kind.
    pub fn process(process: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            timestamp: now_ms(),
            process: process.into(),
            group: None,
            content: content.into(),
            source: LogSource::Process,
            output_kind: OutputKind::Combined,
        }
    }

    /// Create a new process log line from stdout.
    pub fn stdout(process: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            timestamp: now_ms(),
            process: process.into(),
            group: None,
            content: content.into(),
            source: LogSource::Process,
            output_kind: OutputKind::Stdout,
        }
    }

    /// Create a new process log line from stderr.
    pub fn stderr(process: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            timestamp: now_ms(),
            process: process.into(),
            group: None,
            content: content.into(),
            source: LogSource::Process,
            output_kind: OutputKind::Stderr,
        }
    }

    /// Create a new daemon log line.
    pub fn daemon(process: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            timestamp: now_ms(),
            process: process.into(),
            group: None,
            content: content.into(),
            source: LogSource::Daemon,
            output_kind: OutputKind::Combined,
        }
    }

    /// Set the group for this log line.
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Set the output kind for this log line.
    pub fn with_output_kind(mut self, output_kind: OutputKind) -> Self {
        self.output_kind = output_kind;
        self
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// API request from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiRequest {
    /// Get overall status (one-shot).
    Status,

    /// Subscribe to status updates (streaming).
    /// Server will send StatusUpdate messages until client disconnects.
    Subscribe,

    /// List all groups.
    ListGroups,

    /// Get logs for a process.
    GetLogs {
        /// Process name.
        name: String,
        /// Number of lines to return (None = all).
        lines: Option<usize>,
    },

    /// Subscribe to logs for a process (streaming).
    SubscribeLogs {
        /// Process name.
        name: String,
    },

    /// Restart a specific group.
    RestartGroup {
        /// Group name.
        name: String,
    },

    /// Restart a specific process (task or daemon) within a group.
    RestartProcess {
        /// Group name.
        group: String,
        /// Process name (task or daemon).
        process: String,
    },

    /// Restart all groups.
    RestartAll,

    /// Reload configuration.
    ReloadConfig,

    /// Graceful shutdown.
    Shutdown,
}

/// Internal command sent from server to engine.
pub struct EngineCommand {
    /// The API request.
    pub request: ApiRequest,
    /// Channel to send the response back.
    pub response_tx: oneshot::Sender<ApiResponse>,
}

/// API response from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiResponse {
    /// Success with optional message.
    Ok { message: Option<String> },

    /// Status response (one-shot).
    Status { state: DaemonState },

    /// Status update (streaming).
    StatusUpdate { state: DaemonState },

    /// Log lines (one-shot).
    Logs { name: String, lines: Vec<LogLine> },

    /// Log line (streaming).
    Log(LogLine),

    /// Error response.
    Error { message: String },

    /// End of stream marker.
    EndOfStream,
}

impl ApiResponse {
    /// Create a success response.
    pub fn ok() -> Self {
        Self::Ok { message: None }
    }

    /// Create a success response with a message.
    pub fn ok_with_message(message: impl Into<String>) -> Self {
        Self::Ok {
            message: Some(message.into()),
        }
    }

    /// Create an error response.
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        // Simple request
        let req = ApiRequest::Status;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"status"}"#);

        // Request with field
        let req = ApiRequest::RestartGroup {
            name: "web".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"restart_group""#));
        assert!(json.contains(r#""name":"web""#));
    }

    #[test]
    fn test_request_deserialization() {
        let json = r#"{"type":"status"}"#;
        let req: ApiRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, ApiRequest::Status));

        let json = r#"{"type":"restart_group","name":"api"}"#;
        let req: ApiRequest = serde_json::from_str(json).unwrap();
        match req {
            ApiRequest::RestartGroup { name } => assert_eq!(name, "api"),
            _ => panic!("expected RestartGroup"),
        }

        let json = r#"{"type":"get_logs","name":"server","lines":100}"#;
        let req: ApiRequest = serde_json::from_str(json).unwrap();
        match req {
            ApiRequest::GetLogs { name, lines } => {
                assert_eq!(name, "server");
                assert_eq!(lines, Some(100));
            }
            _ => panic!("expected GetLogs"),
        }
    }

    #[test]
    fn test_response_serialization() {
        // Ok response
        let resp = ApiResponse::ok();
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"ok""#));

        // Ok with message
        let resp = ApiResponse::ok_with_message("done");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""message":"done""#));

        // Error response
        let resp = ApiResponse::error("something failed");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"error""#));
        assert!(json.contains(r#""message":"something failed""#));
    }

    #[test]
    fn test_response_deserialization() {
        let json = r#"{"type":"ok","message":null}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(matches!(resp, ApiResponse::Ok { message: None }));

        let json = r#"{"type":"error","message":"not found"}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        match resp {
            ApiResponse::Error { message } => assert_eq!(message, "not found"),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_all_request_variants() {
        // Ensure all variants can be serialized and deserialized
        let requests = vec![
            ApiRequest::Status,
            ApiRequest::Subscribe,
            ApiRequest::ListGroups,
            ApiRequest::GetLogs {
                name: "test".to_string(),
                lines: None,
            },
            ApiRequest::SubscribeLogs {
                name: "test".to_string(),
            },
            ApiRequest::RestartGroup {
                name: "test".to_string(),
            },
            ApiRequest::RestartAll,
            ApiRequest::ReloadConfig,
            ApiRequest::Shutdown,
        ];

        for req in requests {
            let json = serde_json::to_string(&req).unwrap();
            let parsed: ApiRequest = serde_json::from_str(&json).unwrap();
            // Verify round-trip works by re-serializing
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }
}
