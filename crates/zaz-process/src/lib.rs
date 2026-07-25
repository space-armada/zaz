//! Process management for zaz.
//!
//! Handles command execution, process groups, signals, and PTY allocation.

mod error;
mod executor;
mod launcher;
mod pty;
mod service;
mod signal;
mod task;

pub use error::ProcessError;
pub use executor::{CommandOutput, Executor, OutputLine, StreamingRun};
pub use launcher::{DaemonLauncher, LaunchHandle};
pub use pty::ManagedChild;
pub use service::{HookRun, KillAction, Service, ServiceExitInfo, ServiceState, StopEscalation};
pub use signal::SignalHandler;
pub use task::TaskRunner;
