//! Service process management.

use crate::executor::{CommandOutput, OutputLine, StreamingRun};
use crate::pty::ManagedChild;
use crate::{Executor, ProcessError, SignalHandler};
use nix::sys::signal::Signal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zaz_config::{ServiceCommand, Silence};

/// Minimum restart delay.
const MIN_RESTART_DELAY: Duration = Duration::from_millis(500);

/// Maximum restart delay.
const MAX_RESTART_DELAY: Duration = Duration::from_secs(8);

/// Multiplier for exponential backoff.
const BACKOFF_MULTIPLIER: u32 = 2;

/// Grace period before escalating to SIGKILL when a service sets no `stop_timeout`.
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Gap between `ready_check` runs when a service sets no `ready_poll_interval`.
const DEFAULT_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a `ready_check` may keep failing when a service sets no `ready_timeout`.
///
/// Longer than the stop timeout because legitimate startup times vary far more widely than
/// shutdowns do. A JVM warming up or a container running an entrypoint script would trip a
/// ten-second window that a graceful shutdown never gets near.
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How many characters of a failed probe's output the readiness timeout message carries.
const READY_FAILURE_OUTPUT_LIMIT: usize = 400;

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

/// A lifecycle hook process the daemon started for this service.
///
/// The run itself is driven by a detached task, so a cancelled poll can never drop the
/// child. The service keeps only what the stop deadline needs: the process group to
/// signal, and the flag that task raises once the run is over.
struct TrackedHook {
    pgid: u32,
    finished: Arc<AtomicBool>,
}

/// A lifecycle hook handed to the caller to drive to completion.
///
/// Driving it elsewhere is what keeps a cancelled poll from dropping the hook's child. The
/// service that issued this handle already recorded the process group, so it can still kill
/// a hook that outlives the stop timeout.
pub struct HookRun {
    run: StreamingRun,

    // The guard lives on the handle rather than inside `stream`, so a driver dropped before
    // it was ever polled still clears the flag. Reporting a hook nobody is driving as
    // running would stall every later poll of the service.
    _guard: FinishedOnDrop,
}

impl HookRun {
    /// Drive the hook to completion, streaming its output through the channel.
    pub async fn stream(
        self,
        output_tx: mpsc::UnboundedSender<OutputLine>,
    ) -> Result<CommandOutput, ProcessError> {
        let _guard = self._guard;

        self.run.stream(output_tx).await
    }
}

/// Raises a hook's finished flag however its run ends, including a dropped driver.
struct FinishedOnDrop(Arc<AtomicBool>);

impl Drop for FinishedOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A readiness probe the daemon started for this service.
///
/// The run is driven by a detached task, so only what a later tick needs stays here: the
/// process group to kill if the window closes on a probe still running, the flags that task
/// raises, and whatever the probe printed if it reported not-ready.
struct TrackedProbe {
    pgid: Option<u32>,
    finished: Arc<AtomicBool>,
    passed: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
}

impl TrackedProbe {
    /// Take what this probe printed when it failed, leaving the slot empty.
    fn take_failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// The window a service spends spawned but not yet reporting ready.
///
/// Modelled on `stop_deadline`: the window lives on the service and `poll_readiness` decides
/// against a caller-supplied `now`, so readiness needs no clock of its own and stays testable
/// without one.
struct ReadyState {
    started_at: Instant,
    deadline: Instant,
    next_attempt: Instant,
    probe: Option<TrackedProbe>,
    attempts: u32,

    /// What the most recent failing probe printed, kept across probes for the message a
    /// readiness timeout logs.
    last_failure: Option<String>,
}

/// What a readiness tick calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyPoll {
    /// The service has no `ready_check`, or its readiness is already settled.
    Idle,

    /// Still inside the window, with no probe due this tick.
    Waiting,

    /// A probe is due. The caller expands `ready_check` and hands the run back through
    /// `begin_ready_probe`.
    Due {
        /// True for the first probe of this window, which is where the check is worth naming
        /// in the log. Naming it on all three hundred would drown out the service's own
        /// output.
        first: bool,
    },

    /// A probe exited zero. The service is ready.
    Passed {
        /// How long the service spent waiting to report ready.
        waited: Duration,
    },

    /// The window closed with no probe reporting ready.
    TimedOut {
        /// How long the service was given before it was given up on.
        waited: Duration,

        /// What the last failing probe printed, absent when no probe ever finished.
        last_failure: Option<String>,
    },
}

/// A readiness probe handed to the caller to drive to completion.
///
/// The poll branch that starts a probe is cancelled whenever an API command arrives, and an
/// attached TUI asks for status twice a second. A probe awaited there would be dropped
/// mid-run on every one of those, so any check slower than that cadence would never finish.
/// Driving it from a detached task is what makes a slow check workable at all.
///
/// The outcome lands in the flags the service kept rather than being returned, since the
/// tick that reads it is a later one than the tick that started the run.
pub struct ReadyProbeRun {
    run: StreamingRun,
    passed: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,

    // Same placement as `HookRun`: on the handle, so a driver dropped before it was ever
    // polled still clears the flag and lets the next tick start a fresh probe.
    _guard: FinishedOnDrop,
}

impl ReadyProbeRun {
    /// Drive the probe to completion and record whether the service reported ready.
    ///
    /// A zero exit means ready. Every other ending means not yet, and what the check printed
    /// is kept for the message a readiness timeout logs.
    pub async fn run(self) {
        let Self {
            run,
            passed,
            failure,
            _guard,
        } = self;

        let message = match run.collect().await {
            Ok(output) if output.exit_code == Some(0) => {
                passed.store(true, Ordering::Release);
                return;
            }
            Ok(output) => describe_failed_probe(&output),
            Err(e) => format!("could not run: {}", e),
        };

        *failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
    }
}

/// Render a probe that reported not-ready: how it ended, then what it printed.
///
/// An operator reading a readiness timeout wants the check's own words. A bare exit code
/// says the check failed without saying what it saw.
fn describe_failed_probe(output: &CommandOutput) -> String {
    let ending = match output.exit_code {
        Some(code) => format!("exit code {}", code),
        None => "killed by a signal".to_string(),
    };

    let printed: Vec<&str> = output
        .stdout
        .iter()
        .chain(output.stderr.iter())
        .map(String::as_str)
        .filter(|line| !line.trim().is_empty())
        .collect();

    if printed.is_empty() {
        return ending;
    }

    let joined = printed.join("; ");
    let truncated: String = joined.chars().take(READY_FAILURE_OUTPUT_LIMIT).collect();
    let ellipsis = if truncated.len() < joined.len() {
        "..."
    } else {
        ""
    };

    format!("{}: {}{}", ending, truncated, ellipsis)
}

/// The kill step an expired stop timeout reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillAction {
    /// SIGKILL went to the service's own process group.
    SentSignal,

    /// The service configures `kill_command`. The caller runs it as a hook.
    RunKillCommand,
}

/// What a service's expired stop timeout called for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopEscalation {
    /// A hook was still running when the timeout expired, so its process group was killed
    /// before the service's own kill step.
    pub killed_hook: bool,

    /// The kill step taken against the service, absent when the service was already gone or
    /// the signal could not be delivered.
    pub action: Option<KillAction>,
}

/// Manages a long-running service process.
pub struct Service {
    config: ServiceCommand,
    executor: Executor,
    child: Option<ManagedChild>,
    state: ServiceState,
    restart_delay: Duration,
    last_start: Option<Instant>,
    stop_deadline: Option<Instant>,
    hook: Option<TrackedHook>,
    ready: Option<ReadyState>,
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
            stop_deadline: None,
            hook: None,
            ready: None,
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

    /// Get the configured stop command template, before variable expansion.
    ///
    /// Returns None when the service is stopped by a signal to its process group.
    pub fn stop_command_template(&self) -> Option<&str> {
        self.config.stop_command.as_deref()
    }

    /// Get the configured kill command template, before variable expansion.
    ///
    /// Returns None when the service is force killed by a signal to its process group.
    pub fn kill_command_template(&self) -> Option<&str> {
        self.config.kill_command.as_deref()
    }

    /// Get the configured readiness check template, before variable expansion.
    ///
    /// Returns None when the service counts as ready the moment it spawns.
    pub fn ready_check_template(&self) -> Option<&str> {
        self.config.ready_check.as_deref()
    }

    /// How long the service waits between `ready_check` runs.
    pub fn ready_poll_interval(&self) -> Duration {
        self.config
            .ready_poll_interval
            .map(|t| t.as_duration())
            .unwrap_or(DEFAULT_READY_POLL_INTERVAL)
    }

    /// How long the service's `ready_check` may keep failing before it is given up on.
    pub fn ready_timeout(&self) -> Duration {
        self.config
            .ready_timeout
            .map(|t| t.as_duration())
            .unwrap_or(DEFAULT_READY_TIMEOUT)
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

    /// Spawn a stop or kill hook with the given fully expanded command, recording its
    /// process group so a hook that outlives the stop timeout can be killed.
    ///
    /// The returned handle is driven by the caller rather than here, since a hook outlives
    /// the poll tick that started it. The cleanup hook does not go through this: it is
    /// awaited before a spawn, when no stop timeout is running.
    pub fn begin_hook(&mut self, command: &str) -> Result<HookRun, ProcessError> {
        let run = self.executor.spawn_streaming(command)?;
        let finished = Arc::new(AtomicBool::new(false));

        if let Some(pgid) = run.pgid() {
            self.hook = Some(TrackedHook {
                pgid,
                finished: Arc::clone(&finished),
            });
        }

        Ok(HookRun {
            run,
            _guard: FinishedOnDrop(finished),
        })
    }

    /// Returns true while a stop or kill hook this service started is still running.
    pub fn has_running_hook(&self) -> bool {
        self.hook
            .as_ref()
            .is_some_and(|hook| !hook.finished.load(Ordering::Acquire))
    }

    /// Start the stop timeout without sending a signal, for a service whose `stop_command`
    /// replaced the signal.
    ///
    /// Mirrors the guard in `stop`: a service with no live child owes no exit, so arming a
    /// deadline against it would escalate at nothing.
    pub fn arm_stop_deadline(&mut self) {
        if self.child.as_ref().and_then(|child| child.id()).is_none() {
            return;
        }

        self.ready = None;
        self.stop_deadline = Some(Instant::now() + self.stop_timeout());
    }

    /// Open a readiness window for a service that just spawned. Does nothing for a service
    /// with no `ready_check`, which counts as ready the moment it starts.
    ///
    /// The first probe is due immediately. A service that is already accepting work should
    /// not have to sit through a poll interval before it can say so.
    pub fn arm_readiness(&mut self) {
        if self.config.ready_check.is_none() {
            return;
        }

        let now = Instant::now();

        self.ready = Some(ReadyState {
            started_at: now,
            deadline: now + self.ready_timeout(),
            next_attempt: now,
            probe: None,
            attempts: 0,
            last_failure: None,
        });
    }

    /// Returns true while the service is spawned but has not yet reported ready.
    pub fn awaiting_readiness(&self) -> bool {
        self.ready.is_some()
    }

    /// Close the readiness window for a check that cannot be run at all, such as one whose
    /// variables do not expand or whose shell will not spawn.
    ///
    /// Neither gets better inside the window, so polling on to the timeout would only repeat
    /// the same failure every interval. The caller decides what the service's status becomes.
    pub fn abandon_readiness(&mut self) {
        self.kill_running_probe();
        self.ready = None;
    }

    /// Decide what an open readiness window owes this tick, against a caller-supplied `now`.
    ///
    /// Pure and non-awaiting, the same shape as `enforce_stop_deadline`. The probe itself
    /// runs detached, so all this does is read the flags its driver raised and hand back the
    /// next step.
    ///
    /// Both `Passed` and `TimedOut` close the window, so readiness settles once per spawn and
    /// a service given up on is not polled forever. A restart opens a fresh window.
    ///
    /// A probe that passed wins over a deadline reached in the same tick. The check did
    /// report ready, and the window is there to bound waiting, not to overrule an answer that
    /// is already in.
    ///
    /// A window that has never dispatched a probe cannot time out. A `ready_timeout` too
    /// short to contain one poll tick still gets its one check.
    pub fn poll_readiness(&mut self, now: Instant) -> ReadyPoll {
        let Some(ready) = self.ready.as_mut() else {
            return ReadyPoll::Idle;
        };

        let waited = now.saturating_duration_since(ready.started_at);
        let expired = now >= ready.deadline;

        let mut probe_running = false;
        if let Some(probe) = ready.probe.take() {
            if !probe.finished.load(Ordering::Acquire) {
                probe_running = true;
                ready.probe = Some(probe);
            } else if probe.passed.load(Ordering::Acquire) {
                self.ready = None;
                return ReadyPoll::Passed { waited };
            } else if let Some(failure) = probe.take_failure() {
                ready.last_failure = Some(failure);
            }
        }

        // A window owes one probe before it can close. A `ready_timeout` shorter than a poll
        // tick, zero included, means run the check and give up, not give up without running
        // it. Once the probe is dispatched the deadline applies as usual, so a check that
        // hangs is still killed rather than holding the window open.
        if !expired || ready.attempts == 0 {
            if probe_running || now < ready.next_attempt {
                return ReadyPoll::Waiting;
            }

            return ReadyPoll::Due {
                first: ready.attempts == 0,
            };
        }

        let last_failure = ready.last_failure.take();

        // A probe outliving the window it belongs to is orphaned the moment the window
        // closes, so its process group goes with it. Leaving it would leak a shell per
        // service that never came up.
        self.kill_running_probe();
        self.ready = None;

        ReadyPoll::TimedOut {
            waited,
            last_failure,
        }
    }

    /// Spawn a readiness probe with the given fully expanded command, recording its process
    /// group so a probe still running when the window closes can be killed.
    ///
    /// The returned handle is driven by the caller rather than here, since a probe outlives
    /// the poll tick that started it.
    ///
    /// The next attempt is scheduled from the start of this probe rather than from its end,
    /// which makes the interval a floor on the gap between starts. A probe slower than its
    /// interval simply runs back to back. Probes never overlap either way, since a window
    /// holding one reports `Waiting` until it finishes.
    pub fn begin_ready_probe(&mut self, command: &str) -> Result<ReadyProbeRun, ProcessError> {
        let next_attempt = Instant::now() + self.ready_poll_interval();
        if let Some(ready) = self.ready.as_mut() {
            ready.next_attempt = next_attempt;
            ready.attempts += 1;
        }

        let run = self.executor.spawn_streaming(command)?;
        let finished = Arc::new(AtomicBool::new(false));
        let passed = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));

        if let Some(ready) = self.ready.as_mut() {
            ready.probe = Some(TrackedProbe {
                pgid: run.pgid(),
                finished: Arc::clone(&finished),
                passed: Arc::clone(&passed),
                failure: Arc::clone(&failure),
            });
        }

        Ok(ReadyProbeRun {
            run,
            passed,
            failure,
            _guard: FinishedOnDrop(finished),
        })
    }

    /// Kill the process group of a readiness probe that is still running. Returns true if
    /// one was.
    fn kill_running_probe(&self) -> bool {
        let Some(probe) = self.ready.as_ref().and_then(|ready| ready.probe.as_ref()) else {
            return false;
        };

        if probe.finished.load(Ordering::Acquire) {
            return false;
        }

        let Some(pgid) = probe.pgid else {
            return false;
        };

        tracing::warn!(
            name = %self.config.name(),
            pgid = pgid,
            "ready timeout expired with a check still running, killing it"
        );

        if let Err(e) = SignalHandler::send_to_group(pgid as i32, Signal::SIGKILL) {
            tracing::warn!(
                name = %self.config.name(),
                pgid = pgid,
                error = %e,
                "could not kill readiness check; it has most likely already exited"
            );
            return false;
        }

        true
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
        self.stop_deadline = None;
        self.ready = None;

        Ok(())
    }

    /// How long the service gets to exit after a stop signal before it is force killed.
    pub fn stop_timeout(&self) -> Duration {
        self.config
            .stop_timeout
            .map(|t| t.as_duration())
            .unwrap_or(DEFAULT_STOP_TIMEOUT)
    }

    /// Returns true while the service owes an exit within its stop timeout.
    pub fn has_stop_deadline(&self) -> bool {
        self.stop_deadline.is_some()
    }

    /// Send restart signal to the service.
    pub fn signal_restart(&mut self) -> Result<(), ProcessError> {
        if let Some(child) = &self.child {
            if let Some(pid) = child.id() {
                let signal = SignalHandler::to_nix_signal(self.config.signal());
                tracing::info!(
                    name = %self.config.name(),
                    pid = pid,
                    signal = ?signal,
                    "sending restart signal"
                );
                SignalHandler::send_to_group(pid as i32, signal)?;
                self.ready = None;
                self.stop_deadline = Some(Instant::now() + self.stop_timeout());
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
                self.ready = None;
                self.stop_deadline = Some(Instant::now() + self.stop_timeout());
            }
        }

        Ok(())
    }

    /// Force kill the service if its stop timeout has elapsed. Returns what the expired
    /// deadline called for, or None while the service is still inside its window.
    ///
    /// The deadline is armed by whichever of `signal_restart`, `stop`, or `arm_stop_deadline`
    /// began the stop, and is one-shot. It clears whether or not the kill lands, so a caller
    /// polling this cannot spin forever on a process it is unable to kill.
    ///
    /// A hook still running at this point outlived the window the whole stop is bounded by,
    /// so its process group is killed first. Leaving it would reproduce, one level removed,
    /// the orphan it exists to prevent.
    ///
    /// A signal that fails is logged rather than returned. The group can exit between the
    /// caller's liveness check and this signal, and a stop must not be blocked by a service
    /// that is already gone.
    ///
    /// The child stays so the ordinary `check` path reaps the exit and schedules the
    /// restart. Dropping it here would strand a restarting service with no exit for anyone
    /// to observe.
    pub fn enforce_stop_deadline(&mut self, now: Instant) -> Option<StopEscalation> {
        let deadline = self.stop_deadline?;

        if now < deadline {
            return None;
        }

        self.stop_deadline = None;

        let killed_hook = self.kill_running_hook();

        // A service that already exited needs no kill step. Running `kill_command` against
        // it would report a force kill of something that stopped on its own.
        if !self.is_running() {
            return Some(StopEscalation {
                killed_hook,
                action: None,
            });
        }

        tracing::warn!(
            name = %self.config.name(),
            timeout_ms = self.stop_timeout().as_millis(),
            "stop timeout expired, force killing service"
        );

        if self.config.kill_command.is_some() {
            return Some(StopEscalation {
                killed_hook,
                action: Some(KillAction::RunKillCommand),
            });
        }

        let action = self.force_kill().then_some(KillAction::SentSignal);

        Some(StopEscalation {
            killed_hook,
            action,
        })
    }

    /// Send SIGKILL to the service's process group. Returns true if it was delivered.
    ///
    /// A signal that fails is logged rather than returned. The group can exit between a
    /// caller's liveness check and this signal, and a stop must not be blocked by a service
    /// that is already gone.
    ///
    /// The child stays so the ordinary `check` path reaps the exit and schedules the
    /// restart. Dropping it here would strand a restarting service with no exit for anyone
    /// to observe.
    pub fn force_kill(&self) -> bool {
        let Some(pid) = self.child.as_ref().and_then(|child| child.id()) else {
            return false;
        };

        tracing::warn!(name = %self.config.name(), pid = pid, "force killing service");

        if let Err(e) = SignalHandler::send_to_group(pid as i32, Signal::SIGKILL) {
            tracing::warn!(
                name = %self.config.name(),
                pid = pid,
                error = %e,
                "could not force kill service; it has most likely already exited"
            );
            return false;
        }

        true
    }

    /// Kill the process group of a hook that is still running. Returns true if one was.
    fn kill_running_hook(&mut self) -> bool {
        let Some(hook) = self.hook.as_ref() else {
            return false;
        };

        if hook.finished.load(Ordering::Acquire) {
            return false;
        }

        tracing::warn!(
            name = %self.config.name(),
            pgid = hook.pgid,
            "stop timeout expired with a lifecycle hook still running, killing it"
        );

        if let Err(e) = SignalHandler::send_to_group(hook.pgid as i32, Signal::SIGKILL) {
            tracing::warn!(
                name = %self.config.name(),
                pgid = hook.pgid,
                error = %e,
                "could not kill lifecycle hook; it has most likely already exited"
            );
            return false;
        }

        true
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
                self.stop_deadline = None;
                self.ready = None;
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
    use std::path::Path;

    fn service_with_cleanup(command: &str, cleanup: Option<&str>) -> ServiceCommand {
        let mut config = ServiceCommand::new("svc", command);
        config.cleanup_command = cleanup.map(str::to_string);
        config
    }

    /// Build a service whose only way out is the escalation to SIGKILL.
    fn stubborn_service(stop_timeout: Option<Duration>) -> Service {
        let mut config = ServiceCommand::new("svc", "unused");
        config.no_pty = true;
        config.stop_timeout = stop_timeout.map(zaz_config::HumanDuration::new);

        Service::new(config, Executor::new(Some("/bin/sh".to_string())))
    }

    /// A command that ignores SIGTERM, touching `ready` once the trap is installed.
    ///
    /// Signalling before the marker appears races the shell's own startup, and the default
    /// disposition kills it outright before the trap takes effect.
    fn stubborn_command(ready: &Path) -> String {
        format!(
            "trap '' TERM; : > '{}'; while true; do sleep 1; done",
            ready.display()
        )
    }

    /// Poll until the stubborn service has installed its SIGTERM trap.
    async fn wait_until_trapping(ready: &Path) {
        for _ in 0..200 {
            if ready.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        panic!("service never installed its SIGTERM trap");
    }

    /// Poll `enforce_stop_deadline` until it escalates, or give up after a second.
    async fn wait_for_escalation(service: &mut Service) -> Option<StopEscalation> {
        for _ in 0..100 {
            if let Some(escalation) = service.enforce_stop_deadline(Instant::now()) {
                return Some(escalation);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        None
    }

    /// Poll `check` until the service reports its exit, or give up after a second.
    async fn wait_for_exit(service: &mut Service) -> Option<ServiceExitInfo> {
        for _ in 0..100 {
            if let Some(info) = service.check().await.unwrap() {
                return Some(info);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        None
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

    #[test]
    fn test_unset_stop_timeout_falls_back_to_the_default() {
        let service = stubborn_service(None);

        assert_eq!(service.stop_timeout(), DEFAULT_STOP_TIMEOUT);
    }

    #[test]
    fn test_configured_stop_timeout_overrides_the_default() {
        let service = stubborn_service(Some(Duration::from_secs(45)));

        assert_eq!(service.stop_timeout(), Duration::from_secs(45));
    }

    #[test]
    fn test_no_deadline_is_armed_until_a_signal_is_sent() {
        let mut service = stubborn_service(Some(Duration::from_millis(50)));

        assert!(!service.has_stop_deadline());
        assert!(service.enforce_stop_deadline(Instant::now()).is_none());
    }

    #[tokio::test]
    async fn test_restart_signal_escalates_to_sigkill_after_the_stop_timeout() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let ready = temp_dir.path().join("trapping");

        let mut service = stubborn_service(Some(Duration::from_millis(100)));
        service.start(&stubborn_command(&ready)).unwrap();
        wait_until_trapping(&ready).await;

        service.signal_restart().unwrap();
        assert!(service.has_stop_deadline());

        let escalation = wait_for_escalation(&mut service)
            .await
            .expect("stop timeout never escalated to SIGKILL");
        assert_eq!(escalation.action, Some(KillAction::SentSignal));
        assert!(
            !service.has_stop_deadline(),
            "the deadline must clear so the escalation stays one-shot"
        );

        let exit = wait_for_exit(&mut service)
            .await
            .expect("SIGKILLed service never reported its exit");
        assert_eq!(
            exit.exit_code, None,
            "a signalled exit carries no exit code, so SIGKILL is what ended it"
        );
    }

    #[tokio::test]
    async fn test_service_that_exits_within_the_timeout_never_escalates() {
        let mut service = stubborn_service(Some(Duration::from_secs(30)));
        service.start("sleep 30").unwrap();

        service.signal_restart().unwrap();

        let exit = wait_for_exit(&mut service)
            .await
            .expect("service ignored its restart signal");
        assert_eq!(exit.exit_code, None);
        assert!(
            !service.has_stop_deadline(),
            "an observed exit must disarm the deadline"
        );
        assert!(service.enforce_stop_deadline(Instant::now()).is_none());
    }

    #[tokio::test]
    async fn test_stop_arms_the_same_deadline_as_a_restart() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let ready = temp_dir.path().join("trapping");

        let mut service = stubborn_service(Some(Duration::from_millis(100)));
        service.start(&stubborn_command(&ready)).unwrap();
        wait_until_trapping(&ready).await;

        service.stop().unwrap();
        assert!(service.has_stop_deadline());

        let escalation = wait_for_escalation(&mut service)
            .await
            .expect("shutdown-path stop never escalated to SIGKILL");
        assert_eq!(escalation.action, Some(KillAction::SentSignal));

        let exit = wait_for_exit(&mut service)
            .await
            .expect("SIGKILLed service never reported its exit");
        assert_eq!(exit.exit_code, None);
    }

    #[tokio::test]
    async fn test_arm_stop_deadline_escalates_without_a_signal() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let ready = temp_dir.path().join("trapping");

        let mut service = stubborn_service(Some(Duration::from_millis(100)));
        service.start(&stubborn_command(&ready)).unwrap();
        wait_until_trapping(&ready).await;

        service.arm_stop_deadline();
        assert!(service.has_stop_deadline());

        let escalation = wait_for_escalation(&mut service)
            .await
            .expect("a hook-armed deadline never escalated");
        assert_eq!(escalation.action, Some(KillAction::SentSignal));
        assert!(!escalation.killed_hook);
    }

    #[test]
    fn test_arm_stop_deadline_ignores_a_service_with_no_child() {
        let mut service = stubborn_service(Some(Duration::from_millis(50)));

        service.arm_stop_deadline();

        assert!(!service.has_stop_deadline());
    }

    #[tokio::test]
    async fn test_a_hook_reports_itself_finished_once_its_run_ends() {
        let mut service = stubborn_service(None);

        let hook = service.begin_hook("exit 0").unwrap();
        assert!(service.has_running_hook());

        let (tx, _rx) = mpsc::unbounded_channel();
        let output = hook.stream(tx).await.unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert!(!service.has_running_hook());
    }

    #[tokio::test]
    async fn test_a_dropped_hook_driver_stops_reporting_the_hook_as_running() {
        let mut service = stubborn_service(None);

        let hook = service.begin_hook("sleep 30").unwrap();
        assert!(service.has_running_hook());

        let (tx, _rx) = mpsc::unbounded_channel();
        drop(hook.stream(tx));

        assert!(!service.has_running_hook());
    }

    #[tokio::test]
    async fn test_a_hook_outliving_the_stop_timeout_is_killed_first() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let ready = temp_dir.path().join("trapping");

        let mut service = stubborn_service(Some(Duration::from_millis(100)));
        service.start(&stubborn_command(&ready)).unwrap();
        wait_until_trapping(&ready).await;

        let hook = service.begin_hook("sleep 30").unwrap();
        service.arm_stop_deadline();

        let (tx, _rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(hook.stream(tx));

        let escalation = wait_for_escalation(&mut service)
            .await
            .expect("a service held up by its own stop hook never escalated");
        assert!(escalation.killed_hook);
        assert_eq!(escalation.action, Some(KillAction::SentSignal));

        let output = driver
            .await
            .expect("the hook driver panicked")
            .expect("the killed hook reported no exit");
        assert_eq!(
            output.exit_code, None,
            "a signalled exit carries no exit code, so SIGKILL is what ended the hook"
        );
    }

    #[tokio::test]
    async fn test_a_kill_command_replaces_the_escalation_signal() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let ready = temp_dir.path().join("trapping");

        let mut config = ServiceCommand::new("svc", "unused");
        config.no_pty = true;
        config.stop_timeout = Some(zaz_config::HumanDuration::new(Duration::from_millis(100)));
        config.kill_command = Some("true".to_string());
        let mut service = Service::new(config, Executor::new(Some("/bin/sh".to_string())));

        service.start(&stubborn_command(&ready)).unwrap();
        wait_until_trapping(&ready).await;
        service.arm_stop_deadline();

        let escalation = wait_for_escalation(&mut service)
            .await
            .expect("a service with a kill command never escalated");
        assert_eq!(escalation.action, Some(KillAction::RunKillCommand));
        assert!(
            service.is_running(),
            "the escalation must leave the kill to the hook rather than signal the group"
        );
    }

    #[tokio::test]
    async fn test_a_deadline_reached_after_the_service_exited_takes_no_kill_step() {
        let mut service = stubborn_service(Some(Duration::from_millis(50)));
        service.start("exit 0").unwrap();
        service.arm_stop_deadline();

        for _ in 0..100 {
            if !service.is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let escalation = wait_for_escalation(&mut service)
            .await
            .expect("an expired deadline must still report itself");
        assert_eq!(escalation.action, None);
        assert!(!escalation.killed_hook);
    }

    /// Build a service whose readiness window is under the test's control.
    fn ready_service(check: Option<&str>, interval: Option<u64>, timeout: Option<u64>) -> Service {
        let mut config = ServiceCommand::new("svc", "sleep 30");
        config.no_pty = true;
        config.ready_check = check.map(str::to_string);
        config.ready_poll_interval = interval.map(zaz_config::HumanDuration::from_millis);
        config.ready_timeout = timeout.map(zaz_config::HumanDuration::from_millis);

        Service::new(config, Executor::new(Some("/bin/sh".to_string())))
    }

    /// Start a probe and drive it to completion, the way the daemon's detached task does.
    async fn probe(service: &mut Service, command: &str) {
        service.begin_ready_probe(command).unwrap().run().await;
    }

    #[test]
    fn test_a_service_without_a_ready_check_never_opens_a_window() {
        let mut service = ready_service(None, None, None);
        service.arm_readiness();

        assert!(!service.awaiting_readiness());
        assert_eq!(service.poll_readiness(Instant::now()), ReadyPoll::Idle);
    }

    #[test]
    fn test_unset_ready_tuning_falls_back_to_the_defaults() {
        let service = ready_service(Some("true"), None, None);

        assert_eq!(service.ready_poll_interval(), Duration::from_millis(100));
        assert_eq!(service.ready_timeout(), Duration::from_secs(30));
    }

    #[test]
    fn test_configured_ready_tuning_overrides_the_defaults() {
        let service = ready_service(Some("true"), Some(250), Some(5_000));

        assert_eq!(service.ready_poll_interval(), Duration::from_millis(250));
        assert_eq!(service.ready_timeout(), Duration::from_secs(5));
    }

    #[test]
    fn test_arming_readiness_makes_the_first_probe_due_immediately() {
        let mut service = ready_service(Some("true"), None, None);
        service.arm_readiness();

        assert!(service.awaiting_readiness());
        assert_eq!(
            service.poll_readiness(Instant::now()),
            ReadyPoll::Due { first: true },
            "a service that is already up must not wait out an interval to say so"
        );
    }

    #[tokio::test]
    async fn test_a_passing_probe_settles_readiness() {
        let mut service = ready_service(Some("true"), None, None);
        service.arm_readiness();

        probe(&mut service, "true").await;

        assert!(matches!(
            service.poll_readiness(Instant::now()),
            ReadyPoll::Passed { .. }
        ));
        assert!(
            !service.awaiting_readiness(),
            "readiness settles once per spawn, so the window must close behind a pass"
        );
    }

    #[tokio::test]
    async fn test_a_failing_probe_waits_out_the_interval_before_the_next_one() {
        let mut service = ready_service(Some("false"), Some(500), None);
        service.arm_readiness();

        probe(&mut service, "false").await;

        let now = Instant::now();
        assert_eq!(service.poll_readiness(now), ReadyPoll::Waiting);
        assert_eq!(
            service.poll_readiness(now + Duration::from_millis(500)),
            ReadyPoll::Due { first: false },
            "only the first probe of a window names the check in the log"
        );
    }

    #[tokio::test]
    async fn test_a_probe_still_running_holds_off_a_second_one() {
        let mut service = ready_service(Some("sleep 30"), Some(1), None);
        service.arm_readiness();

        let _running = service.begin_ready_probe("sleep 30").unwrap();

        assert_eq!(
            service.poll_readiness(Instant::now() + Duration::from_secs(1)),
            ReadyPoll::Waiting,
            "probes must never overlap, however short the interval"
        );
    }

    #[tokio::test]
    async fn test_a_closed_window_reports_what_the_last_failing_check_printed() {
        let mut service = ready_service(Some("unused"), None, Some(50));
        service.arm_readiness();

        probe(&mut service, "echo not listening yet >&2; exit 7").await;

        let ReadyPoll::TimedOut { last_failure, .. } =
            service.poll_readiness(Instant::now() + Duration::from_secs(60))
        else {
            panic!("a window past its deadline must time out");
        };

        let failure = last_failure.expect("the last failing probe's output must survive");
        assert!(
            failure.contains("exit code 7") && failure.contains("not listening yet"),
            "an operator needs the check's own words, got {:?}",
            failure
        );
        assert!(!service.awaiting_readiness());
    }

    #[tokio::test]
    async fn test_a_probe_outliving_its_window_is_killed() {
        let mut service = ready_service(Some("sleep 30"), None, Some(50));
        service.arm_readiness();

        let running = service.begin_ready_probe("sleep 30").unwrap();

        assert!(matches!(
            service.poll_readiness(Instant::now() + Duration::from_secs(60)),
            ReadyPoll::TimedOut { .. }
        ));

        tokio::time::timeout(Duration::from_secs(5), running.run())
            .await
            .expect("the closed window left its probe running, leaking a shell");
    }

    #[tokio::test]
    async fn test_a_pass_wins_a_deadline_reached_in_the_same_tick() {
        let mut service = ready_service(Some("true"), None, Some(50));
        service.arm_readiness();

        probe(&mut service, "true").await;

        assert!(
            matches!(
                service.poll_readiness(Instant::now() + Duration::from_secs(60)),
                ReadyPoll::Passed { .. }
            ),
            "the window bounds waiting; it must not overrule an answer already in"
        );
    }

    #[test]
    fn test_a_zero_ready_timeout_still_runs_its_one_check() {
        let mut service = ready_service(Some("true"), None, Some(0));
        service.arm_readiness();

        assert_eq!(
            service.poll_readiness(Instant::now() + Duration::from_secs(60)),
            ReadyPoll::Due { first: true },
            "a zero timeout means one check then give up, not give up without checking"
        );
    }

    #[tokio::test]
    async fn test_a_zero_ready_timeout_honors_a_check_that_passes() {
        let mut service = ready_service(Some("true"), None, Some(0));
        service.arm_readiness();

        probe(&mut service, "true").await;

        assert!(matches!(
            service.poll_readiness(Instant::now() + Duration::from_secs(60)),
            ReadyPoll::Passed { .. }
        ));
    }

    #[tokio::test]
    async fn test_a_zero_ready_timeout_gives_up_after_its_one_check() {
        let mut service = ready_service(Some("unused"), None, Some(0));
        service.arm_readiness();

        probe(&mut service, "exit 7").await;

        let ReadyPoll::TimedOut { last_failure, .. } =
            service.poll_readiness(Instant::now() + Duration::from_secs(60))
        else {
            panic!("the one check failed, so the window must close");
        };

        let failure = last_failure.expect("the check did run, so its verdict must survive");
        assert!(
            failure.contains("exit code 7"),
            "an operator needs the check's own words, got {:?}",
            failure
        );
    }

    #[tokio::test]
    async fn test_a_reaped_exit_closes_the_readiness_window() {
        let mut service = ready_service(Some("true"), None, None);
        service.start("exit 0").unwrap();
        service.arm_readiness();

        wait_for_exit(&mut service)
            .await
            .expect("the service never reported its exit");

        assert!(
            !service.awaiting_readiness(),
            "a service that exited is not going to report ready; its restart opens a new window"
        );
    }

    #[test]
    fn test_abandoning_readiness_closes_the_window() {
        let mut service = ready_service(Some("true"), None, None);
        service.arm_readiness();

        service.abandon_readiness();

        assert!(!service.awaiting_readiness());
        assert_eq!(service.poll_readiness(Instant::now()), ReadyPoll::Idle);
    }

    #[tokio::test]
    async fn test_a_stop_closes_the_readiness_window() {
        let mut service = ready_service(Some("true"), None, None);
        service.start("sleep 30").unwrap();
        service.arm_readiness();

        service.stop().unwrap();

        assert!(
            !service.awaiting_readiness(),
            "a service on its way down owes no readiness"
        );
    }
}
