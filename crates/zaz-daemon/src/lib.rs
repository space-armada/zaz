//! Background daemon and API server for zaz.
//!
//! Provides a Unix socket API for controlling zaz and querying state.

mod api;
mod dependency;
mod engine;
mod error;
mod log_layer;
mod log_storage;
mod log_storage_sqlite;
mod log_store;
pub mod notify;
mod server;
mod state;

pub use api::{ApiRequest, ApiResponse, EngineCommand, LogLine, LogSource, OutputKind};
pub use engine::Engine;
pub use error::{DaemonError, LogStorageError};
pub use log_layer::DaemonLogLayer;
pub use log_storage::{LogQuery, LogQueryResult, LogStorage, LogStorageStats};
pub use server::{discover_config_upward, resolve_socket, socket_path_for_config, Client, Server};
pub use state::{DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus};
