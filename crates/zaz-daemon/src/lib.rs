//! Background daemon and API server for zaz.
//!
//! Provides a Unix socket API for controlling zaz and querying state.

mod api;
mod engine;
mod error;
mod log_layer;
mod log_store;
pub mod notify;
mod server;
mod state;

pub use api::{ApiRequest, ApiResponse, EngineCommand, LogLine, LogSource, OutputKind};
pub use engine::Engine;
pub use error::DaemonError;
pub use log_layer::DaemonLogLayer;
pub use server::{default_socket_path, socket_path_for_config, Client, Server};
pub use state::{DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus};
