//! Process management for zaz.
//!
//! Handles command execution, process groups, signals, and PTY allocation.

mod daemon;
mod error;
mod executor;
mod pty;
mod signal;
mod task;

pub use daemon::{Daemon, DaemonState};
pub use error::ProcessError;
pub use executor::{CommandOutput, Executor, OutputLine};
pub use pty::ManagedChild;
pub use signal::SignalHandler;
pub use task::TaskRunner;
