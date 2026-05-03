//! Process management for zaz.
//!
//! Handles command execution, process groups, signals, and PTY allocation.

mod daemon;
mod error;
mod executor;
mod launcher;
mod pty;
mod signal;
mod task;

pub use daemon::{Daemon, DaemonExitInfo, DaemonState};
pub use error::ProcessError;
pub use executor::{CommandOutput, Executor, OutputLine};
pub use launcher::{DaemonLauncher, LaunchHandle};
pub use pty::ManagedChild;
pub use signal::SignalHandler;
pub use task::TaskRunner;
