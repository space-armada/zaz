//! MCP tool server for zaz.
//!
//! Exposes a stdio-transport MCP server that AI assistants can use to query
//! daemon state and trigger control operations.

mod client;
mod error;
mod server;
mod types;

pub use error::McpError;
pub use server::{run, McpRunOptions, ZazMcpServer};
pub use types::{
    ConfigDaemon, ConfigGroup, ConfigReport, ConfigSettings, ConfigTask, DaemonStatusReport,
    GroupReport, GroupStatusReport, GroupSummary, GroupsReport, LogEntry, LogFormatReport,
    LogSourceReport, LogsReport, LogsRequest, MutationReport, OutputKindReport, ProcessKind,
    ProcessReport, ProcessStatusReport, RestartGroupRequest, RestartProcessRequest, SignalReport,
    SilenceReport, StatusReport,
};
