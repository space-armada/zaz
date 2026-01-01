//! Process management for zaz.
//!
//! Handles command execution, process groups, signals, and PTY allocation.

mod daemon;
mod error;
mod executor;
mod prep;
mod signal;

pub use daemon::{Daemon, DaemonState};
pub use error::ProcessError;
pub use executor::{CommandOutput, Executor};
pub use prep::PrepRunner;
pub use signal::SignalHandler;
