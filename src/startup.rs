//! How launchers wait on a daemon they started or found.
//!
//! A daemon answers `Status` as soon as it is bound, which is before it has loaded its config.
//! Any reply therefore no longer means ready. Each launcher reads the reply body and applies its
//! own wait policy:
//!
//! - [`wait_for_first_reply`] returns on the first reply and leaves the daemon up. TUI autostart,
//!   MCP autostart, and supervisor member attach use it.
//! - [`wait_for_startup`] waits for the outcome. `zaz start`, `zaz start` with several configs,
//!   and the pre-checks that find a daemon still starting use it.

use anyhow::Result;
use std::path::Path;
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use zaz_daemon::{ApiRequest, ApiResponse, Client, DaemonState, DaemonStatus, LoadError};
use zaz_process::LaunchHandle;

/// Per-daemon polling budget for the first reply. Each attempt sleeps [`DAEMON_READY_INTERVAL`]
/// before probing the socket, so the window is attempts * interval. Sized to tolerate slow
/// debug-build starts under load, where a healthy daemon can take several seconds to bind its
/// socket. Too tight a window kills or misses a legitimately-slow daemon, which surfaces as leaks
/// and flaky startup failures.
pub(crate) const DAEMON_READY_ATTEMPTS: usize = 100;
pub(crate) const DAEMON_READY_INTERVAL: Duration = Duration::from_millis(100);

/// What a probe of a daemon socket found.
#[derive(Debug)]
pub(crate) enum DaemonAvailability {
    /// A daemon answered `Status`. Its state says whether it has finished starting.
    Responding(Box<DaemonState>),

    /// The socket accepted a connection, but no daemon answered.
    StaleSocket,

    /// Nothing is listening.
    Unreachable,
}

impl DaemonAvailability {
    pub(crate) fn is_responding(&self) -> bool {
        matches!(self, Self::Responding(_))
    }
}

pub(crate) async fn check_daemon_availability(
    socket_path: &Path,
    timeout: Duration,
) -> DaemonAvailability {
    match tokio::time::timeout(timeout, Client::connect(socket_path)).await {
        Ok(Ok(mut client)) => {
            match tokio::time::timeout(timeout, client.request(&ApiRequest::Status)).await {
                Ok(Ok(ApiResponse::Status { state })) => {
                    DaemonAvailability::Responding(Box::new(state))
                }
                // Anything else is a daemon that is not serving. One that is tearing down answers
                // "engine unavailable" to requests already in flight, and must not read as ready.
                Ok(Ok(other)) => {
                    tracing::debug!(
                        socket = %socket_path.display(),
                        response = ?other,
                        "daemon answered a status request with something other than its state"
                    );
                    DaemonAvailability::StaleSocket
                }
                Ok(Err(e)) => {
                    tracing::debug!(
                        socket = %socket_path.display(),
                        error = %e,
                        "socket accepted connection but daemon status request failed"
                    );
                    DaemonAvailability::StaleSocket
                }
                Err(_) => {
                    tracing::debug!(
                        socket = %socket_path.display(),
                        timeout_ms = timeout.as_millis(),
                        "socket accepted connection but daemon status request timed out"
                    );
                    DaemonAvailability::StaleSocket
                }
            }
        }
        Ok(Err(e)) => {
            tracing::debug!(
                socket = %socket_path.display(),
                error = %e,
                "failed to connect to daemon socket"
            );
            DaemonAvailability::Unreachable
        }
        Err(_) => {
            tracing::debug!(
                socket = %socket_path.display(),
                timeout_ms = timeout.as_millis(),
                "timed out connecting to daemon socket"
            );
            DaemonAvailability::Unreachable
        }
    }
}

pub(crate) async fn is_daemon_responsive(socket_path: &Path, timeout: Duration) -> bool {
    check_daemon_availability(socket_path, timeout)
        .await
        .is_responding()
}

#[derive(Debug)]
pub(crate) enum FirstReply {
    Replied(Box<DaemonState>),
    Crashed(ExitStatus),
    Timeout,
}

/// Wait for a daemon this launcher spawned to answer `Status` once, whatever the answer says.
pub(crate) async fn wait_for_first_reply(
    socket_path: &Path,
    handle: &mut LaunchHandle,
    attempts: usize,
    delay: Duration,
) -> Result<FirstReply> {
    for _ in 0..attempts {
        tokio::time::sleep(delay).await;
        if let Some(status) = handle.try_wait()? {
            return Ok(FirstReply::Crashed(status));
        }
        if let DaemonAvailability::Responding(state) =
            check_daemon_availability(socket_path, Duration::from_secs(1)).await
        {
            return Ok(FirstReply::Replied(state));
        }
    }

    Ok(FirstReply::Timeout)
}

/// How a wait for the outcome of a startup ended.
#[derive(Debug)]
pub(crate) enum StartupOutcome {
    /// The daemon left `Starting`. The state is its last reply.
    Ready(Box<DaemonState>),

    /// The daemon reported that its first load failed.
    LoadFailed(LoadError),

    /// The process this launcher spawned exited.
    Crashed(ExitStatus),

    /// The daemon stopped answering for the hang budget.
    Hung,
}

/// Timing of a wait for the outcome of a startup.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StartupWait {
    /// Pause between probes.
    pub interval: Duration,

    /// How long requests may go unanswered before the daemon counts as hung. A daemon that
    /// answers `Starting` is never hung, however long the evaluation behind it takes.
    pub hang_after: Duration,

    /// How often to print a progress line while the daemon reports `Starting`.
    pub progress_every: Duration,
}

impl Default for StartupWait {
    fn default() -> Self {
        Self {
            interval: DAEMON_READY_INTERVAL,
            hang_after: Duration::from_secs(10),
            progress_every: Duration::from_secs(2),
        }
    }
}

/// Wait for a daemon to finish starting, with no fixed bound while it keeps answering `Starting`.
///
/// `handle` is the process this launcher spawned, when it spawned one. Its exit is a crash. A
/// daemon found already running has no handle.
pub(crate) async fn wait_for_startup(
    socket_path: &Path,
    mut handle: Option<&mut LaunchHandle>,
    wait: &StartupWait,
) -> Result<StartupOutcome> {
    let begun = Instant::now();
    let mut last_reply = Instant::now();
    let mut last_progress = Instant::now();

    loop {
        tokio::time::sleep(wait.interval).await;

        if let Some(handle) = handle.as_mut() {
            if let Some(status) = handle.try_wait()? {
                return Ok(StartupOutcome::Crashed(status));
            }
        }

        match check_daemon_availability(socket_path, Duration::from_secs(1)).await {
            DaemonAvailability::Responding(state) => {
                last_reply = Instant::now();

                if let Some(error) = state.load_error {
                    return Ok(StartupOutcome::LoadFailed(error));
                }
                if state.status != DaemonStatus::Starting {
                    return Ok(StartupOutcome::Ready(state));
                }

                if last_progress.elapsed() >= wait.progress_every {
                    last_progress = Instant::now();
                    eprintln!(
                        "waiting for the daemon to finish loading its configuration ({}s)",
                        begun.elapsed().as_secs()
                    );
                }
            }
            _ if last_reply.elapsed() >= wait.hang_after => return Ok(StartupOutcome::Hung),
            _ => {}
        }
    }
}

/// Print a daemon's coded load error to stderr.
pub(crate) fn print_load_error(error: &LoadError) {
    eprintln!("Error: the daemon failed to load its configuration:");
    for line in error.message.lines() {
        eprintln!("  {line}");
    }
    if let Some(hint) = &error.hint {
        eprintln!("       hint: {hint}");
    }
}

/// Shut down a daemon this launcher spawned, and make sure its process is gone.
pub(crate) async fn shut_down_spawned(socket_path: &Path, handle: &mut LaunchHandle) {
    if let Ok(mut client) = Client::connect(socket_path).await {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            client.request(&ApiRequest::Shutdown),
        )
        .await;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(handle.try_wait(), Ok(Some(_))) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = handle.kill();
}

/// What a pre-check found when it waited on a daemon it did not spawn.
#[derive(Debug)]
pub(crate) enum Existing {
    Settled(Box<DaemonState>),
    LoadFailed(LoadError),
    Hung,
}

/// Wait on a daemon a pre-check found, which may still be starting. Never shuts it down.
pub(crate) async fn wait_on_existing(
    socket_path: &Path,
    state: Box<DaemonState>,
    wait: &StartupWait,
) -> Result<Existing> {
    if let Some(error) = state.load_error {
        return Ok(Existing::LoadFailed(error));
    }
    if state.status != DaemonStatus::Starting {
        return Ok(Existing::Settled(state));
    }

    Ok(match wait_for_startup(socket_path, None, wait).await? {
        StartupOutcome::Ready(state) => Existing::Settled(state),
        StartupOutcome::LoadFailed(error) => Existing::LoadFailed(error),
        StartupOutcome::Hung | StartupOutcome::Crashed(_) => Existing::Hung,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn quick() -> StartupWait {
        StartupWait {
            interval: Duration::from_millis(10),
            hang_after: Duration::from_millis(400),
            progress_every: Duration::from_secs(60),
        }
    }

    fn status(status: DaemonStatus, load_error: Option<LoadError>) -> ApiResponse {
        ApiResponse::Status {
            state: DaemonState {
                status,
                load_error,
                ..Default::default()
            },
        }
    }

    /// A daemon that answers each `Status` with the next scripted reply, and repeats the last.
    /// A script of `None` never answers.
    fn fake_daemon(socket: &Path, script: Option<Vec<ApiResponse>>) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        let script = Arc::new(Mutex::new(script.map(VecDeque::from)));

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let script = Arc::clone(&script);

                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();

                    while let Ok(Some(_)) = lines.next_line().await {
                        let reply = {
                            let mut guard = script.lock().unwrap();
                            match guard.as_mut() {
                                None => None,
                                Some(queue) if queue.len() > 1 => queue.pop_front(),
                                Some(queue) => queue.front().cloned(),
                            }
                        };

                        let Some(reply) = reply else {
                            std::future::pending::<()>().await;
                            return;
                        };
                        let body = serde_json::to_string(&reply).unwrap();
                        let _ = writer.write_all(body.as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;
                    }
                });
            }
        })
    }

    #[tokio::test]
    async fn a_starting_daemon_is_waited_on_until_it_runs() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("d.sock");
        let _daemon = fake_daemon(
            &socket,
            Some(vec![
                status(DaemonStatus::Starting, None),
                status(DaemonStatus::Starting, None),
                status(DaemonStatus::Running, None),
            ]),
        );

        let outcome = wait_for_startup(&socket, None, &quick()).await.unwrap();

        assert!(
            matches!(outcome, StartupOutcome::Ready(state) if state.status == DaemonStatus::Running)
        );
    }

    #[tokio::test]
    async fn a_daemon_that_keeps_answering_starting_is_not_hung() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("d.sock");
        let _daemon = fake_daemon(&socket, Some(vec![status(DaemonStatus::Starting, None)]));

        let waited = tokio::time::timeout(
            Duration::from_millis(1200),
            wait_for_startup(&socket, None, &quick()),
        )
        .await;

        assert!(
            waited.is_err(),
            "the wait outlasts the hang budget while replies keep coming"
        );
    }

    #[tokio::test]
    async fn a_load_error_ends_the_wait_with_the_coded_error() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("d.sock");
        let error = LoadError::new("env_file_missing", "no such file");
        let _daemon = fake_daemon(
            &socket,
            Some(vec![
                status(DaemonStatus::Starting, None),
                status(DaemonStatus::Starting, Some(error.clone())),
            ]),
        );

        let outcome = wait_for_startup(&socket, None, &quick()).await.unwrap();

        assert!(matches!(outcome, StartupOutcome::LoadFailed(found) if found == error));
    }

    #[tokio::test]
    async fn unanswered_requests_for_the_budget_are_a_hang() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("d.sock");
        let _daemon = fake_daemon(&socket, None);

        let outcome = wait_for_startup(&socket, None, &quick()).await.unwrap();

        assert!(matches!(outcome, StartupOutcome::Hung));
    }

    #[tokio::test]
    async fn a_pre_check_waits_on_a_starting_daemon_without_shutting_it_down() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("d.sock");
        let _daemon = fake_daemon(
            &socket,
            Some(vec![
                status(DaemonStatus::Starting, None),
                status(DaemonStatus::Running, None),
            ]),
        );
        let state = Box::new(DaemonState::default());

        let existing = wait_on_existing(&socket, state, &quick()).await.unwrap();

        assert!(
            matches!(existing, Existing::Settled(state) if state.status == DaemonStatus::Running)
        );
    }

    #[tokio::test]
    async fn a_pre_check_that_finds_a_load_error_does_not_wait() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("never-bound.sock");
        let error = LoadError::new("config_parse_error", "bad toml");
        let state = Box::new(DaemonState {
            load_error: Some(error.clone()),
            ..Default::default()
        });

        let existing = wait_on_existing(&socket, state, &quick()).await.unwrap();

        assert!(matches!(existing, Existing::LoadFailed(found) if found == error));
    }

    #[tokio::test]
    async fn a_reply_that_is_not_a_state_does_not_read_as_ready() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("d.sock");
        let _daemon = fake_daemon(
            &socket,
            Some(vec![ApiResponse::error("engine unavailable")]),
        );

        let found = check_daemon_availability(&socket, Duration::from_secs(1)).await;

        assert!(
            matches!(found, DaemonAvailability::StaleSocket),
            "{found:?}"
        );
    }

    #[tokio::test]
    async fn the_probe_reads_the_state_body() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("d.sock");
        let _daemon = fake_daemon(&socket, Some(vec![status(DaemonStatus::Starting, None)]));

        let found = check_daemon_availability(&socket, Duration::from_secs(1)).await;

        assert!(
            matches!(found, DaemonAvailability::Responding(state) if state.status == DaemonStatus::Starting)
        );
    }
}
