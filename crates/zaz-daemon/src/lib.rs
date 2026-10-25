//! Background daemon and API server for zaz.
//!
//! Provides a Unix socket API for controlling zaz and querying state.

mod api;
mod dependency;
mod engine;
mod env;
mod env_command;
mod env_report;
mod error;
mod evaluate;
mod host;
mod log_layer;
mod log_storage;
mod log_storage_sqlite;
mod log_store;
pub mod notify;
mod server;
mod sources;
mod state;
mod target;

pub use api::{
    is_unknown_request_error, ApiRequest, ApiResponse, EngineCommand, LogLine, LogSource,
    OutputKind,
};
pub use engine::Engine;
pub use env_command::{EnvCommandError, EnvCommandErrorKind};
pub use env_report::{
    EnvLevel, EnvReport, EnvSourceKind, EnvSourceReport, EnvStart, GroupEnvReport, ProcessEnvReport,
};
pub use error::{DaemonError, LogStorageError};
pub use host::{DaemonHost, RunOutcome};
pub use log_layer::DaemonLogLayer;
pub use log_storage::{LogQuery, LogQueryResult, LogStorage, LogStorageStats};
pub use server::{
    discover_config_upward, discover_workspace_root_upward, resolve_target,
    resolve_workspace_socket, socket_path_for_config, Client, Server,
};
pub use state::{
    DaemonState, DaemonStatus, GroupState, GroupStatus, LoadError, MemberError, ProcessState,
    ProcessStatus,
};
pub use target::{ConfigOrigin, DaemonTarget};
