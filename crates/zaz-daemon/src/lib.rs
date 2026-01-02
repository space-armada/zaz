//! Background daemon and API server for zaz.
//!
//! Provides a Unix socket API for controlling zaz and querying state.

mod api;
mod engine;
mod error;
mod log_layer;
mod server;
mod state;

pub use api::{ApiRequest, ApiResponse, EngineCommand, LogLine, LogSource};
pub use engine::Engine;
pub use error::DaemonError;
pub use log_layer::DaemonLogLayer;
pub use server::{default_socket_path, Client, Server};
pub use state::{DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus};
