//! Process management for zaz.
//!
//! Handles command execution, process groups, signals, and PTY allocation.

mod daemon;
mod error;
mod executor;
mod signal;
mod task;

pub use daemon::{Daemon, DaemonState};
pub use error::ProcessError;
pub use executor::{CommandOutput, Executor};
pub use signal::SignalHandler;
pub use task::TaskRunner;
