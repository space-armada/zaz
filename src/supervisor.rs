//! Workspace supervisor: owns an ad-hoc working set of single-config child
//! daemons.
//!
//! The supervisor spawns one ordinary `zaz daemon` per member config (ADR-0007),
//! adopting a member whose hashed socket is already live rather than killing it.
//! The single-config engine is untouched: each member keeps its own socket, so
//! member-directory commands still resolve to the member's own daemon. This
//! milestone lands the lifecycle spine; namespaced routing and aggregate reads
//! arrive in later milestones.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc;

use zaz_daemon::{
    socket_path_for_config, ApiRequest, ApiResponse, Client, DaemonState, DaemonStatus,
    EngineCommand, Server,
};
use zaz_process::LaunchHandle;

use crate::{
    check_daemon_availability, wait_for_daemon_ready, DaemonAvailability, DaemonReadyOutcome,
};

/// A member of the working set. `handle` is `Some` only for children the
/// supervisor spawned; an adopted member's daemon predates the supervisor and is
/// left running on teardown.
struct Member {
    config_path: PathBuf,
    socket_path: PathBuf,
    handle: Option<LaunchHandle>,
    adopted: bool,
}

/// Owns the working set of member daemons.
pub(crate) struct Supervisor {
    workspace_socket: PathBuf,
    debug: bool,
    members: Vec<Member>,
}

impl Supervisor {
    fn new(workspace_socket: PathBuf, debug: bool) -> Self {
        Self {
            workspace_socket,
            debug,
            members: Vec::new(),
        }
    }

    fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Per-child output log path, derived from the member socket so concurrent
    /// children do not collide. Placed alongside the workspace socket so crash
    /// output stays discoverable.
    fn child_output_log(&self, member_socket: &Path) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        member_socket.hash(&mut hasher);
        let hash = hasher.finish();
        let dir = self
            .workspace_socket
            .parent()
            .unwrap_or_else(|| Path::new("."));
        dir.join(format!("child-{:016x}.log", hash))
    }

    /// Attach a member: validate the config, then adopt its daemon if the
    /// hashed socket is already live, otherwise spawn one. A failure returns
    /// `Err` without touching the rest of the working set.
    async fn attach(&mut self, config_path: &Path) -> Result<()> {
        if !config_path.exists() {
            bail!("config file not found: {}", config_path.display());
        }
        zaz_config::load(config_path)
            .with_context(|| format!("invalid config: {}", config_path.display()))?;

        let canonical = config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.to_path_buf());
        if self.members.iter().any(|m| m.config_path == canonical) {
            bail!("config already attached: {}", config_path.display());
        }

        let socket = socket_path_for_config(config_path);
        let timeout = Duration::from_secs(1);

        if matches!(
            check_daemon_availability(&socket, timeout).await,
            DaemonAvailability::Running
        ) {
            tracing::info!(
                config = %config_path.display(),
                socket = %socket.display(),
                "adopting live member daemon"
            );
            self.members.push(Member {
                config_path: canonical,
                socket_path: socket,
                handle: None,
                adopted: true,
            });
            return Ok(());
        }

        let output_log = self.child_output_log(&socket);
        if let Some(parent) = output_log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut handle =
            crate::start_daemon_via_launcher(config_path, &socket, self.debug, None, &output_log)?;

        match wait_for_daemon_ready(&socket, &mut handle, 20, Duration::from_millis(100)).await? {
            DaemonReadyOutcome::Ready => {
                tracing::info!(
                    config = %config_path.display(),
                    socket = %socket.display(),
                    pid = handle.id(),
                    "spawned member daemon"
                );
                self.members.push(Member {
                    config_path: canonical,
                    socket_path: socket,
                    handle: Some(handle),
                    adopted: false,
                });
                Ok(())
            }
            DaemonReadyOutcome::Crashed(status) => bail!(
                "member daemon for {} exited before becoming ready (status: {}); see {}",
                config_path.display(),
                status,
                output_log.display()
            ),
            DaemonReadyOutcome::Timeout => bail!(
                "member daemon for {} did not become ready within 2s; see {}",
                config_path.display(),
                output_log.display()
            ),
        }
    }

    /// Stop every spawned child; adopted members are dropped without stopping.
    /// A spawned child is stopped cleanly; an adopted member's daemon is left
    /// running, honoring adopt's "don't kill" intent.
    async fn detach_all(&mut self) {
        let members = std::mem::take(&mut self.members);
        for member in members {
            if member.adopted {
                continue;
            }
            stop_child(&member.socket_path).await;
        }
    }

    /// Log children that have exited so a crashed member is visible without
    /// taking down the rest of the set.
    fn reap_children(&mut self) {
        for member in &mut self.members {
            if let Some(handle) = member.handle.as_mut() {
                if let Ok(Some(status)) = handle.try_wait() {
                    tracing::warn!(
                        config = %member.config_path.display(),
                        %status,
                        "member daemon exited"
                    );
                    member.handle = None;
                }
            }
        }
    }

    /// Map an API request to a response and whether it should trigger shutdown.
    /// Aggregate status content arrives in a later milestone; for now `Status`
    /// answers the readiness probe with a minimal running state.
    fn handle_request(&self, request: ApiRequest) -> (ApiResponse, bool) {
        match request {
            ApiRequest::Status | ApiRequest::ListGroups | ApiRequest::Subscribe => {
                let state = DaemonState {
                    status: DaemonStatus::Running,
                    ..Default::default()
                };
                (ApiResponse::Status { state }, false)
            }
            ApiRequest::Shutdown => (
                ApiResponse::ok_with_message("shutting down workspace"),
                true,
            ),
            _ => (
                ApiResponse::error("not supported in workspace mode yet"),
                false,
            ),
        }
    }
}

/// Send `Shutdown` to a child daemon and wait, bounded, for its socket file to
/// disappear so a follow-up command does not race a half-stopped child.
async fn stop_child(socket: &Path) {
    if let Ok(mut client) = Client::connect(socket).await {
        let _ = client.request(&ApiRequest::Shutdown).await;
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !socket.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Run a workspace supervisor in the foreground. Mirrors `run_daemon`: refuse a
/// live workspace socket, run the boot attach loop, then serve a minimal control
/// surface (`Status` for readiness, `Shutdown` to tear the set down).
pub(crate) async fn run_supervisor(
    config_paths: &[PathBuf],
    socket_path: &Path,
    debug: bool,
    quiet: bool,
) -> Result<()> {
    let check_timeout = Duration::from_secs(1);
    if matches!(
        check_daemon_availability(socket_path, check_timeout).await,
        DaemonAvailability::Running
    ) {
        bail!(
            "workspace supervisor already running (socket {} is active)",
            socket_path.display()
        );
    }

    tracing::info!(
        members = config_paths.len(),
        socket = %socket_path.display(),
        quiet,
        "starting workspace supervisor"
    );

    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut supervisor = Supervisor::new(socket_path.to_path_buf(), debug);

    // Boot attach loop. A failed member is logged and skipped; already-attached
    // members stay undisturbed.
    for config_path in config_paths {
        if let Err(e) = supervisor.attach(config_path).await {
            tracing::error!(
                config = %config_path.display(),
                error = %e,
                "failed to attach workspace member"
            );
        }
    }
    tracing::info!(
        attached = supervisor.member_count(),
        "workspace boot attach loop complete"
    );

    let (command_tx, mut command_rx) = mpsc::channel::<EngineCommand>(32);
    let server = Server::bind(socket_path, command_tx).await?;
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            tracing::error!(error = %e, "server error");
        }
    });

    let mut shutdown_requested = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal");
                break;
            }

            Some(cmd) = command_rx.recv() => {
                let (response, is_shutdown) = supervisor.handle_request(cmd.request);
                let _ = cmd.response_tx.send(response);
                if is_shutdown {
                    shutdown_requested = true;
                    break;
                }
            }

            _ = async {
                supervisor.reap_children();
                tokio::time::sleep(Duration::from_millis(200)).await;
            } => {}
        }
    }

    server_handle.abort();
    supervisor.detach_all().await;

    if shutdown_requested {
        tracing::info!("workspace shutdown complete");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_supervisor() -> Supervisor {
        Supervisor::new(PathBuf::from("/tmp/zaz-ws/.zaz/daemon.sock"), false)
    }

    #[test]
    fn child_output_log_is_deterministic_and_distinct() {
        let sup = test_supervisor();
        let a = sup.child_output_log(Path::new("/tmp/a.sock"));
        let b = sup.child_output_log(Path::new("/tmp/b.sock"));

        assert_eq!(a, sup.child_output_log(Path::new("/tmp/a.sock")));
        assert_ne!(a, b);
        assert_eq!(a.parent().unwrap(), Path::new("/tmp/zaz-ws/.zaz"));
        assert!(a
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("child-"));
    }

    #[test]
    fn status_request_answers_running() {
        let sup = test_supervisor();
        let (response, is_shutdown) = sup.handle_request(ApiRequest::Status);
        assert!(!is_shutdown);
        match response {
            ApiResponse::Status { state } => assert_eq!(state.status, DaemonStatus::Running),
            other => panic!("expected Status response, got {:?}", other),
        }
    }

    #[test]
    fn shutdown_request_signals_teardown() {
        let sup = test_supervisor();
        let (response, is_shutdown) = sup.handle_request(ApiRequest::Shutdown);
        assert!(is_shutdown);
        assert!(matches!(response, ApiResponse::Ok { .. }));
    }

    #[test]
    fn unsupported_request_returns_error() {
        let sup = test_supervisor();
        let (response, is_shutdown) = sup.handle_request(ApiRequest::RestartAll);
        assert!(!is_shutdown);
        assert!(matches!(response, ApiResponse::Error { .. }));
    }
}
