//! Process management for zaz.
//!
//! Handles command execution, process groups, signals, and PTY allocation.

mod ansi;
mod capture;
mod error;
mod executor;
mod groups;
mod launcher;
mod pty;
mod service;
mod signal;
mod task;

pub use ansi::strip_ansi;
pub use capture::{capture, CancelHandle, CaptureError, CaptureSpec, Captured, TERMINATE_GRACE};
pub use error::ProcessError;
pub use executor::{CommandOutput, Executor, OutputLine, StreamingRun};
pub use groups::{GroupGuard, GroupRegistry};
pub use launcher::{DaemonLauncher, LaunchHandle};
pub use pty::ManagedChild;
pub use service::{
    HookRun, KillAction, ReadyPoll, ReadyProbeRun, Service, ServiceExitInfo, ServiceState,
    StopEscalation,
};
pub use signal::SignalHandler;
pub use task::TaskRunner;
