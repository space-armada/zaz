//! Daemon process management.

use crate::pty::ManagedChild;
use crate::{Executor, ProcessError, SignalHandler};
use nix::sys::signal::Signal;
use std::time::{Duration, Instant};
use zaz_config::DaemonCommand;

/// Minimum restart delay.
const MIN_RESTART_DELAY: Duration = Duration::from_millis(500);

/// Maximum restart delay.
const MAX_RESTART_DELAY: Duration = Duration::from_secs(8);

/// Multiplier for exponential backoff.
const BACKOFF_MULTIPLIER: u32 = 2;

/// State of a daemon process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// Not yet started.
    Stopped,

    /// Currently running.
    Running,

    /// Waiting to restart after crash.
    Backoff,

    /// Shutting down.
    Stopping,
}

/// Manages a long-running daemon process.
pub struct Daemon {
    config: DaemonCommand,
    executor: Executor,
    child: Option<ManagedChild>,
    state: DaemonState,
    restart_delay: Duration,
    last_start: Option<Instant>,
}

impl Daemon {
    /// Create a new daemon manager.
    pub fn new(config: DaemonCommand, executor: Executor) -> Self {
        Self {
            config,
            executor,
            child: None,
            state: DaemonState::Stopped,
            restart_delay: MIN_RESTART_DELAY,
            last_start: None,
        }
    }

    /// Get the daemon name.
    pub fn name(&self) -> &str {
        self.config.name()
    }

    /// Get the current state.
    pub fn state(&self) -> DaemonState {
        self.state
    }

    /// Get the process ID if running.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Start the daemon.
    pub fn start(&mut self) -> Result<(), ProcessError> {
        if self.state == DaemonState::Running {
            return Ok(());
        }

        tracing::info!(name = %self.config.name(), "starting daemon");

        let child = self
            .executor
            .spawn(&self.config.command, !self.config.no_pty)?;
        self.child = Some(child);
        self.state = DaemonState::Running;
        self.last_start = Some(Instant::now());

        Ok(())
    }

    /// Send restart signal to the daemon.
    pub fn signal_restart(&mut self) -> Result<(), ProcessError> {
        if let Some(child) = &self.child {
            if let Some(pid) = child.id() {
                let signal = SignalHandler::to_nix_signal(self.config.signal);
                tracing::info!(
                    name = %self.config.name(),
                    pid = pid,
                    signal = ?signal,
                    "sending restart signal"
                );
                SignalHandler::send_to_group(pid as i32, signal)?;
            }
        }
        Ok(())
    }

    /// Stop the daemon gracefully (SIGTERM).
    pub fn stop(&mut self) -> Result<(), ProcessError> {
        self.state = DaemonState::Stopping;

        if let Some(child) = &self.child {
            if let Some(pid) = child.id() {
                tracing::info!(name = %self.config.name(), pid = pid, "stopping daemon");
                SignalHandler::send_to_group(pid as i32, Signal::SIGTERM)?;
            }
        }

        Ok(())
    }

    /// Force kill the daemon (SIGKILL).
    pub fn kill(&mut self) -> Result<(), ProcessError> {
        if let Some(child) = &self.child {
            if let Some(pid) = child.id() {
                tracing::warn!(name = %self.config.name(), pid = pid, "force killing daemon");
                SignalHandler::send_to_group(pid as i32, Signal::SIGKILL)?;
            }
        }
        self.child = None;
        self.state = DaemonState::Stopped;
        Ok(())
    }

    /// Check if the daemon is still running.
    pub fn is_running(&mut self) -> bool {
        let Some(child) = &mut self.child else {
            return false;
        };
        // try_wait returns Ok(Some(_)) if exited, Ok(None) if still running
        matches!(child.try_wait(), Ok(None))
    }

    /// Check if the daemon has exited and handle restart logic.
    pub async fn check(&mut self) -> Result<bool, ProcessError> {
        let Some(child) = &mut self.child else {
            return Ok(false);
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                let ran_long = self
                    .last_start
                    .map(|t| t.elapsed() > MAX_RESTART_DELAY)
                    .unwrap_or(false);

                if ran_long || status.success() {
                    // Reset backoff on long run or clean exit
                    self.restart_delay = MIN_RESTART_DELAY;
                } else {
                    // Increase backoff on quick failure
                    self.restart_delay =
                        std::cmp::min(self.restart_delay * BACKOFF_MULTIPLIER, MAX_RESTART_DELAY);
                }

                tracing::info!(
                    name = %self.config.name(),
                    status = ?status,
                    next_delay = ?self.restart_delay,
                    "daemon exited"
                );

                self.child = None;
                self.state = DaemonState::Stopped;
                Ok(true)
            }
            Ok(None) => Ok(false), // Still running
            Err(e) => Err(ProcessError::Spawn(e)),
        }
    }

    /// Get the current restart delay.
    pub fn restart_delay(&self) -> Duration {
        self.restart_delay
    }

    /// Get the startup delay configured for this daemon.
    /// Returns None if no delay is configured.
    pub fn startup_delay(&self) -> Option<Duration> {
        self.config.delay.map(|d| d.as_duration())
    }

    /// Get a reader for PTY output, if available.
    ///
    /// Returns None if:
    /// - The daemon is not running
    /// - The daemon is not using a PTY
    pub fn try_clone_reader(&self) -> Option<Box<dyn std::io::Read + Send>> {
        self.child.as_ref().and_then(|c| c.try_clone_reader())
    }

    /// Check if this daemon uses a PTY.
    pub fn is_pty(&self) -> bool {
        self.child.as_ref().map(|c| c.is_pty()).unwrap_or(false)
    }
}
