//! Service process management.

use crate::executor::{CommandOutput, OutputLine};
use crate::pty::ManagedChild;
use crate::{Executor, ProcessError, SignalHandler};
use nix::sys::signal::Signal;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zaz_config::{ServiceCommand, Silence};

/// Minimum restart delay.
const MIN_RESTART_DELAY: Duration = Duration::from_millis(500);

/// Maximum restart delay.
const MAX_RESTART_DELAY: Duration = Duration::from_secs(8);

/// Multiplier for exponential backoff.
const BACKOFF_MULTIPLIER: u32 = 2;

/// Information about a service that has exited.
#[derive(Debug)]
pub struct ServiceExitInfo {
    /// How long the service was running before it exited.
    pub duration: Duration,
    /// The exit code, if available.
    pub exit_code: Option<i32>,
}

/// State of a service process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Not yet started.
    Stopped,

    /// Currently running.
    Running,

    /// Waiting to restart after crash.
    Backoff,

    /// Shutting down.
    Stopping,
}

/// Manages a long-running service process.
pub struct Service {
    config: ServiceCommand,
    executor: Executor,
    child: Option<ManagedChild>,
    state: ServiceState,
    restart_delay: Duration,
    last_start: Option<Instant>,
}

impl Service {
    /// Create a new service manager.
    pub fn new(config: ServiceCommand, executor: Executor) -> Self {
        Self {
            config,
            executor,
            child: None,
            state: ServiceState::Stopped,
            restart_delay: MIN_RESTART_DELAY,
            last_start: None,
        }
    }

    /// Get the service name.
    pub fn name(&self) -> &str {
        self.config.name()
    }

    /// Get the configured command template, before variable expansion.
    pub fn command_template(&self) -> &str {
        &self.config.command
    }

    /// Get the configured cleanup command template, before variable expansion.
    ///
    /// Returns None when the service has no cleanup hook.
    pub fn cleanup_command_template(&self) -> Option<&str> {
        self.config.cleanup_command.as_deref()
    }

    /// Get the log suppression level configured for this service.
    pub fn silence(&self) -> Silence {
        self.config.silence
    }

    /// Get the current state.
    pub fn state(&self) -> ServiceState {
        self.state
    }

    /// Get the process ID if running.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Run the pre-spawn cleanup hook to completion with the given fully expanded command.
    ///
    /// Output lines stream through `output_tx` as they arrive. The hook runs under the
    /// service's own working directory and environment, which the group-level executor
    /// does not carry.
    ///
    /// The hook always runs without a PTY, so its stdout and stderr stay separable. It also
    /// gets its own process group, which makes it killable by pgid if it hangs.
    pub async fn run_cleanup(
        &self,
        command: &str,
        output_tx: mpsc::UnboundedSender<OutputLine>,
    ) -> Result<CommandOutput, ProcessError> {
        self.executor.run_streaming(command, output_tx).await
    }

    /// Start the service with the given fully expanded command.
    ///
    /// Variable expansion happens at the caller layer so the service does not
    /// need to know about `zaz_vars` or the engine's expansion context.
    pub fn start(&mut self, command: &str) -> Result<(), ProcessError> {
        if self.state == ServiceState::Running {
            return Ok(());
        }

        tracing::info!(name = %self.config.name(), "starting service");

        let child = self.executor.spawn(command, !self.config.no_pty)?;
        self.child = Some(child);
        self.state = ServiceState::Running;
        self.last_start = Some(Instant::now());

        Ok(())
    }

    /// Send restart signal to the service.
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

    /// Stop the service gracefully (SIGTERM).
    pub fn stop(&mut self) -> Result<(), ProcessError> {
        self.state = ServiceState::Stopping;

        if let Some(child) = &self.child {
            if let Some(pid) = child.id() {
                tracing::info!(name = %self.config.name(), pid = pid, "stopping service");
                SignalHandler::send_to_group(pid as i32, Signal::SIGTERM)?;
            }
        }

        Ok(())
    }

    /// Force kill the service (SIGKILL).
    pub fn kill(&mut self) -> Result<(), ProcessError> {
        if let Some(child) = &self.child {
            if let Some(pid) = child.id() {
                tracing::warn!(name = %self.config.name(), pid = pid, "force killing service");
                SignalHandler::send_to_group(pid as i32, Signal::SIGKILL)?;
            }
        }
        self.child = None;
        self.state = ServiceState::Stopped;
        Ok(())
    }

    /// Check if the service is still running.
    pub fn is_running(&mut self) -> bool {
        let Some(child) = &mut self.child else {
            return false;
        };
        // try_wait returns Ok(Some(_)) if exited, Ok(None) if still running
        matches!(child.try_wait(), Ok(None))
    }

    /// Check if the service has exited and handle restart logic.
    ///
    /// Returns `Some(ServiceExitInfo)` if the service has exited, `None` if still running.
    pub async fn check(&mut self) -> Result<Option<ServiceExitInfo>, ProcessError> {
        let Some(child) = &mut self.child else {
            return Ok(None);
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                let duration = self
                    .last_start
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::ZERO);
                let ran_long = duration > MAX_RESTART_DELAY;

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
                    "service exited"
                );

                self.child = None;
                self.state = ServiceState::Stopped;
                Ok(Some(ServiceExitInfo {
                    duration,
                    exit_code: status.code(),
                }))
            }
            Ok(None) => Ok(None), // Still running
            Err(e) => Err(ProcessError::Spawn(e)),
        }
    }

    /// Get the current restart delay.
    pub fn restart_delay(&self) -> Duration {
        self.restart_delay
    }

    /// Get the startup delay configured for this service.
    /// Returns None if no delay is configured.
    pub fn startup_delay(&self) -> Option<Duration> {
        self.config.delay.map(|d| d.as_duration())
    }

    /// Get a reader for PTY output, if available.
    ///
    /// Returns None if:
    /// - The service is not running
    /// - The service is not using a PTY
    pub fn try_clone_reader(&self) -> Option<Box<dyn std::io::Read + Send>> {
        self.child.as_ref().and_then(|c| c.try_clone_reader())
    }

    /// Check if this service uses a PTY.
    pub fn is_pty(&self) -> bool {
        self.child.as_ref().map(|c| c.is_pty()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_with_cleanup(command: &str, cleanup: Option<&str>) -> ServiceCommand {
        let mut config = ServiceCommand::new("svc", command);
        config.cleanup_command = cleanup.map(str::to_string);
        config
    }

    fn drain(mut rx: mpsc::UnboundedReceiver<OutputLine>) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(match line {
                OutputLine::Stdout(s) => format!("out: {}", s),
                OutputLine::Stderr(s) => format!("err: {}", s),
            });
        }
        lines
    }

    #[test]
    fn test_cleanup_command_template_unset_is_none() {
        let service = Service::new(
            service_with_cleanup("sleep 30", None),
            Executor::new(Some("/bin/sh".to_string())),
        );

        assert_eq!(service.cleanup_command_template(), None);
    }

    #[tokio::test]
    async fn test_run_cleanup_streams_both_output_streams() {
        let service = Service::new(
            service_with_cleanup("sleep 30", Some("echo clean; echo noisy >&2")),
            Executor::new(Some("/bin/sh".to_string())),
        );

        let (tx, rx) = mpsc::unbounded_channel();
        let command = service.cleanup_command_template().unwrap().to_string();
        let output = service.run_cleanup(&command, tx).await.unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, vec!["clean".to_string()]);
        assert_eq!(output.stderr, vec!["noisy".to_string()]);
        assert_eq!(
            drain(rx),
            vec!["out: clean".to_string(), "err: noisy".to_string()]
        );
    }

    #[tokio::test]
    async fn test_run_cleanup_reports_nonzero_exit() {
        let service = Service::new(
            service_with_cleanup("sleep 30", Some("exit 3")),
            Executor::new(Some("/bin/sh".to_string())),
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        let output = service.run_cleanup("exit 3", tx).await.unwrap();

        assert_eq!(output.exit_code, Some(3));
    }

    #[tokio::test]
    async fn test_run_cleanup_uses_service_working_dir_and_env() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let canonical = temp_dir.path().canonicalize().unwrap();

        let executor = Executor::new(Some("/bin/sh".to_string()))
            .with_working_dir(canonical.display().to_string())
            .with_env(
                [("ZAZ_CLEANUP_MARKER".to_string(), "marked".to_string())]
                    .into_iter()
                    .collect(),
            );
        let service = Service::new(service_with_cleanup("sleep 30", None), executor);

        let (tx, _rx) = mpsc::unbounded_channel();
        let output = service
            .run_cleanup("pwd; printf '%s\\n' \"$ZAZ_CLEANUP_MARKER\"", tx)
            .await
            .unwrap();

        assert_eq!(
            output.stdout,
            vec![canonical.display().to_string(), "marked".to_string()]
        );
    }
}
