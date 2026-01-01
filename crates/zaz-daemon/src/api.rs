//! API request and response types.

use crate::state::DaemonState;
use serde::{Deserialize, Serialize};

/// API request from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiRequest {
    /// Get overall status.
    Status,

    /// List all groups.
    ListGroups,

    /// Get logs for a process.
    GetLogs {
        /// Process name.
        name: String,
        /// Number of lines to return.
        lines: Option<usize>,
    },

    /// Restart a specific group.
    RestartGroup {
        /// Group name.
        name: String,
    },

    /// Restart all groups.
    RestartAll,

    /// Reload configuration.
    ReloadConfig,

    /// Graceful shutdown.
    Shutdown,
}

/// API response from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiResponse {
    /// Success with optional message.
    Ok { message: Option<String> },

    /// Status response.
    Status { state: DaemonState },

    /// Log lines.
    Logs { name: String, lines: Vec<String> },

    /// Error response.
    Error { message: String },
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
