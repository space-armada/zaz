//! Background daemon and API server for zaz.
//!
//! Provides a Unix socket API for controlling zaz and querying state.

mod api;
mod error;
mod server;
mod state;

pub use api::{ApiRequest, ApiResponse};
pub use error::DaemonError;
pub use server::Server;
pub use state::{DaemonState, DaemonStatus, GroupState, GroupStatus, ProcessState, ProcessStatus};
