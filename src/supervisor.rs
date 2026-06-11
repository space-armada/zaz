//! Workspace supervisor: owns an ad-hoc working set of single-config child
//! daemons.
//!
//! The supervisor spawns one ordinary `zaz daemon` per member config (ADR-0007),
//! adopting a member whose hashed socket is already live rather than killing it.
//! The single-config engine is untouched: each member keeps its own socket, so
//! member-directory commands still resolve to the member's own daemon. Control
//! verbs and log queries route to one member by project token; `Status` fans out
//! and merges every reachable member's groups under qualified names.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc;

use zaz_daemon::{
    socket_path_for_config, ApiRequest, ApiResponse, Client, DaemonState, DaemonStatus,
    EngineCommand, GroupState, GroupStatus, Server,
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
    token: String,
}

/// Resolve a member's project token: the explicit `[settings] name` when set,
/// otherwise the config directory basename. The token is the addressing label in
/// a `project/group` qualified name, so `/` is forbidden in it.
fn resolve_token(config_path: &Path, settings_name: Option<&str>) -> Result<String> {
    if let Some(name) = settings_name {
        if name.is_empty() {
            bail!(
                "[settings] name must not be empty in {}",
                config_path.display()
            );
        }
        if name.contains('/') {
            bail!(
                "[settings] name '{}' must not contain '/' in {}",
                name,
                config_path.display()
            );
        }
        return Ok(name.to_string());
    }

    let basename = config_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    match basename {
        Some(name) if !name.is_empty() && !name.contains('/') => Ok(name.to_string()),
        _ => bail!(
            "cannot derive a project token from {}; set an explicit [settings] name",
            config_path.display()
        ),
    }
}

/// Split a `project/group` qualified name on the first `/`. Both halves must be
/// non-empty; the group half may itself contain `/`.
fn parse_qualified(qualified: &str) -> Result<(&str, &str), String> {
    match qualified.split_once('/') {
        Some((project, rest)) if !project.is_empty() && !rest.is_empty() => Ok((project, rest)),
        _ => Err(format!(
            "malformed qualified name '{}': expected 'project/group'",
            qualified
        )),
    }
}

/// Resolve a routing target into `(project, bare_name)`. A structured `project`
/// field wins; otherwise the `field` string is parsed as `project/name` so the
/// CLI's `project/group` shorthand still routes. The same string thus works
/// against both a supervisor, which splits it here, and a single daemon, which
/// treats it literally because it never calls this.
fn resolve_target(project: Option<String>, field: String) -> Result<(String, String), String> {
    match project {
        Some(p) if !p.is_empty() => Ok((p, field)),
        Some(_) => Err("empty project token".to_string()),
        None => parse_qualified(&field).map(|(p, rest)| (p.to_string(), rest.to_string())),
    }
}

/// The later of two optional timestamps, preferring whichever is present.
fn max_option(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (some, None) | (None, some) => some,
    }
}

/// Combine per-member outcomes of a fan-out operation into one response. `verb`
/// is the success-summary noun phrase, `failures` carries `token: message`
/// strings for members that errored or were unreachable.
fn combine_fanout(verb: &str, ok: usize, total: usize, failures: Vec<String>) -> ApiResponse {
    if total == 0 {
        return ApiResponse::error("no workspace members available");
    }
    if failures.is_empty() {
        return ApiResponse::ok_with_message(format!("{} across {} project(s)", verb, ok));
    }
    ApiResponse::error(format!(
        "{}/{} project(s) failed: {}",
        failures.len(),
        total,
        failures.join("; ")
    ))
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

    fn member_by_token(&self, token: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.token == token)
    }

    /// Reject a working set in which two members resolve to the same project
    /// token: the token is the only addressing label, so a collision makes
    /// routing ambiguous. Resolvable by setting an explicit `[settings] name`.
    fn check_token_uniqueness(&self) -> Result<()> {
        let mut seen: std::collections::HashMap<&str, &Path> = std::collections::HashMap::new();
        for member in &self.members {
            if let Some(first) = seen.insert(member.token.as_str(), &member.config_path) {
                bail!(
                    "workspace members resolve to the same project token '{}': {} and {} (set an explicit [settings] name)",
                    member.token,
                    first.display(),
                    member.config_path.display()
                );
            }
        }
        Ok(())
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
        let config = zaz_config::load(config_path)
            .with_context(|| format!("invalid config: {}", config_path.display()))?;
        let token = resolve_token(config_path, config.settings.name.as_deref())?;

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
            // The socket is live. Adopt only if the daemon there serves this
            // member's own config. A daemon serving anything else means the
            // socket is bound elsewhere, and adopting it would double-manage a
            // config across workspaces; refuse so this member is skipped while
            // the rest of the set and the foreign daemon are left undisturbed.
            match identify_daemon(&socket).await {
                Some(reported) if same_config(&canonical, &reported) => {
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
                        token,
                    });
                    return Ok(());
                }
                Some(reported) => bail!(
                    "member socket {} is bound by a daemon serving {}; refusing to double-manage {}",
                    socket.display(),
                    reported.display(),
                    config_path.display()
                ),
                None => bail!(
                    "member socket {} is bound by an unidentifiable daemon; refusing to adopt {}",
                    socket.display(),
                    config_path.display()
                ),
            }
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
                    token,
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
    /// Mutations and log queries select a member by project token, carried in the
    /// structured `project` field or, when absent, parsed from a `project/group`
    /// string so the CLI shorthand keeps working. `RestartAll`/`ReloadConfig` fan
    /// out across the set, and `Status` merges every reachable member's groups
    /// under qualified names.
    async fn handle_request(&self, request: ApiRequest) -> (ApiResponse, bool) {
        match request {
            ApiRequest::Status | ApiRequest::ListGroups => (self.aggregate_status().await, false),
            ApiRequest::Subscribe => {
                // The supervisor does not stream member status updates; answer the
                // readiness probe with a bare running state.
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
            ApiRequest::RestartGroup { name, project } => {
                (self.route_restart_group(project, name).await, false)
            }
            ApiRequest::RestartProcess {
                group,
                process,
                project,
            } => (
                self.route_restart_process(project, group, process).await,
                false,
            ),
            ApiRequest::RestartAll => (
                self.fan_out("restart initiated for all groups", ApiRequest::RestartAll)
                    .await,
                false,
            ),
            ApiRequest::ReloadConfig => (
                self.fan_out("config reloaded", ApiRequest::ReloadConfig)
                    .await,
                false,
            ),
            ApiRequest::GetLogs {
                name,
                project,
                lines,
                offset,
                limit,
                search,
            } => (
                self.route_logs(project, name, lines, offset, limit, search)
                    .await,
                false,
            ),
            ApiRequest::SubscribeLogs { .. } | ApiRequest::Identify => (
                ApiResponse::error("not supported in workspace mode yet"),
                false,
            ),
        }
    }

    /// Fan `Status` out across the working set and merge each reachable member's
    /// groups under `project/group` keys. An unreachable member surfaces as a
    /// single `Failed` group named after its project, so a crashed child is
    /// visible without taking the aggregate down; the supervisor's own status
    /// stays `Running` so readiness probes never fail.
    async fn aggregate_status(&self) -> ApiResponse {
        let mut merged = DaemonState {
            status: DaemonStatus::Running,
            ..Default::default()
        };

        for member in &self.members {
            match forward(&member.socket_path, &ApiRequest::Status).await {
                Ok(ApiResponse::Status { state }) => {
                    merged.watched_files += state.watched_files;
                    merged.last_change = max_option(merged.last_change, state.last_change);
                    for (group_name, mut group) in state.groups {
                        let qualified = format!("{}/{}", member.token, group_name);
                        group.name = qualified.clone();
                        merged.groups.insert(qualified, group);
                    }
                }
                _ => {
                    let marker = GroupState {
                        name: member.token.clone(),
                        status: GroupStatus::Failed,
                        tasks: Vec::new(),
                        daemons: Vec::new(),
                    };
                    merged.groups.insert(member.token.clone(), marker);
                }
            }
        }

        ApiResponse::Status { state: merged }
    }

    /// Route a group restart to the owning member, returning a response that
    /// re-qualifies the child's bare-name acknowledgement.
    async fn route_restart_group(&self, project: Option<String>, group: String) -> ApiResponse {
        let (project, group) = match resolve_target(project, group) {
            Ok(parts) => parts,
            Err(message) => return ApiResponse::error(message),
        };
        let member = match self.member_by_token(&project) {
            Some(member) => member,
            None => return ApiResponse::error(format!("unknown project '{}'", project)),
        };

        let forwarded = ApiRequest::RestartGroup {
            name: group.clone(),
            project: None,
        };
        match forward(&member.socket_path, &forwarded).await {
            Ok(ApiResponse::Ok { .. }) => ApiResponse::ok_with_message(format!(
                "restart initiated for group '{}/{}'",
                project, group
            )),
            Ok(ApiResponse::Error { message }) => {
                ApiResponse::error(format!("project '{}': {}", project, message))
            }
            Ok(other) => other,
            Err(e) => ApiResponse::error(format!("project '{}': {}", project, e)),
        }
    }

    /// Route a process restart to the owning member. The project is taken from the
    /// structured field or parsed off the `group` string for the CLI shorthand.
    async fn route_restart_process(
        &self,
        project: Option<String>,
        group: String,
        process: String,
    ) -> ApiResponse {
        let (project, _group) = match resolve_target(project, group) {
            Ok(parts) => parts,
            Err(message) => return ApiResponse::error(message),
        };
        let member = match self.member_by_token(&project) {
            Some(member) => member,
            None => return ApiResponse::error(format!("unknown project '{}'", project)),
        };

        let forwarded = ApiRequest::RestartProcess {
            group: _group,
            process: process.clone(),
            project: None,
        };
        match forward(&member.socket_path, &forwarded).await {
            Ok(ApiResponse::Ok { .. }) => {
                ApiResponse::ok_with_message(format!("restarted '{}/{}'", project, process))
            }
            Ok(ApiResponse::Error { message }) => {
                ApiResponse::error(format!("project '{}': {}", project, message))
            }
            Ok(other) => other,
            Err(e) => ApiResponse::error(format!("project '{}': {}", project, e)),
        }
    }

    /// Route a log query to one member daemon. Scoping to a single member is what
    /// keeps a query inside that member's own ZAZ-012 database; rows from another
    /// member are unreachable by construction. A query with no project (and no
    /// `project/name` shorthand) is rejected so a bare `*` cannot fan out.
    #[allow(clippy::too_many_arguments)]
    async fn route_logs(
        &self,
        project: Option<String>,
        name: String,
        lines: Option<usize>,
        offset: Option<usize>,
        limit: Option<usize>,
        search: Option<String>,
    ) -> ApiResponse {
        let (project, name) =
            match resolve_target(project, name) {
                Ok(parts) => parts,
                Err(_) => return ApiResponse::error(
                    "workspace log query requires a project (set `project` or use 'project/name')",
                ),
            };
        let member = match self.member_by_token(&project) {
            Some(member) => member,
            None => return ApiResponse::error(format!("unknown project '{}'", project)),
        };

        let forwarded = ApiRequest::GetLogs {
            name: name.clone(),
            project: None,
            lines,
            offset,
            limit,
            search,
        };
        match forward(&member.socket_path, &forwarded).await {
            Ok(ApiResponse::Logs {
                lines,
                total_count,
                has_more,
                offset,
                ..
            }) => ApiResponse::Logs {
                name: format!("{}/{}", project, name),
                lines,
                total_count,
                has_more,
                offset,
            },
            Ok(ApiResponse::Error { message }) => {
                ApiResponse::error(format!("project '{}': {}", project, message))
            }
            Ok(other) => other,
            Err(e) => ApiResponse::error(format!("project '{}': {}", project, e)),
        }
    }

    /// Forward `request` to every member, combining the outcomes into one
    /// response. Used by the unqualified `RestartAll` and `ReloadConfig` verbs.
    async fn fan_out(&self, verb: &str, request: ApiRequest) -> ApiResponse {
        let mut ok = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for member in &self.members {
            match forward(&member.socket_path, &request).await {
                Ok(ApiResponse::Ok { .. }) => ok += 1,
                Ok(ApiResponse::Error { message }) => {
                    failures.push(format!("{}: {}", member.token, message))
                }
                Ok(_) => failures.push(format!("{}: unexpected response", member.token)),
                Err(e) => failures.push(format!("{}: {}", member.token, e)),
            }
        }
        combine_fanout(verb, ok, self.members.len(), failures)
    }
}

/// Connect to a child daemon socket and issue one request, mapping connection
/// and request errors to display strings the caller re-qualifies.
async fn forward(socket: &Path, request: &ApiRequest) -> Result<ApiResponse, String> {
    let mut client = Client::connect(socket)
        .await
        .map_err(|e| format!("member daemon unreachable: {}", e))?;
    client
        .request(request)
        .await
        .map_err(|e| format!("member request failed: {}", e))
}

/// Ask the daemon bound at `socket` which config it serves, for adopt-time
/// identity verification. Returns the reported config path, or `None` if the
/// socket is unreachable, the request fails, or the response is not `Identity`.
async fn identify_daemon(socket: &Path) -> Option<PathBuf> {
    let mut client = Client::connect(socket).await.ok()?;
    match client.request(&ApiRequest::Identify).await.ok()? {
        ApiResponse::Identity { config_path } => Some(PathBuf::from(config_path)),
        _ => None,
    }
}

/// Whether a daemon reporting `reported` serves the same config as the member at
/// `member`. Both sides are canonicalized so symlinks and relative paths do not
/// produce a false mismatch.
fn same_config(member: &Path, reported: &Path) -> bool {
    let lhs = member
        .canonicalize()
        .unwrap_or_else(|_| member.to_path_buf());
    let rhs = reported
        .canonicalize()
        .unwrap_or_else(|_| reported.to_path_buf());
    lhs == rhs
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

    // Cross-member token uniqueness. Members that failed to attach are already
    // gone; a collision among the survivors aborts startup. Stop the children we
    // just spawned first so the failed boot does not leak daemons.
    if let Err(e) = supervisor.check_token_uniqueness() {
        supervisor.detach_all().await;
        return Err(e);
    }

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
                let (response, is_shutdown) = supervisor.handle_request(cmd.request).await;
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

    fn member(token: &str, config: &str) -> Member {
        Member {
            config_path: PathBuf::from(config),
            socket_path: PathBuf::from(format!("/tmp/{}.sock", token)),
            handle: None,
            adopted: true,
            token: token.to_string(),
        }
    }

    #[test]
    fn resolve_token_prefers_explicit_name() {
        let token = resolve_token(Path::new("/repo/api/zaz.toml"), Some("frontend")).unwrap();
        assert_eq!(token, "frontend");
    }

    #[test]
    fn resolve_token_falls_back_to_basename() {
        let token = resolve_token(Path::new("/repo/api/zaz.toml"), None).unwrap();
        assert_eq!(token, "api");
    }

    #[test]
    fn resolve_token_rejects_empty_and_slashed_name() {
        assert!(resolve_token(Path::new("/repo/api/zaz.toml"), Some("")).is_err());
        assert!(resolve_token(Path::new("/repo/api/zaz.toml"), Some("a/b")).is_err());
    }

    #[test]
    fn parse_qualified_splits_on_first_slash() {
        assert_eq!(parse_qualified("a/b"), Ok(("a", "b")));
        // Group names may legally contain '/'; only the first split matters.
        assert_eq!(parse_qualified("proj/a/b"), Ok(("proj", "a/b")));
    }

    #[test]
    fn parse_qualified_rejects_malformed() {
        assert!(parse_qualified("nogroup").is_err());
        assert!(parse_qualified("/g").is_err());
        assert!(parse_qualified("p/").is_err());
    }

    #[test]
    fn check_token_uniqueness_passes_for_distinct_tokens() {
        let mut sup = test_supervisor();
        sup.members.push(member("api", "/repo/api/zaz.toml"));
        sup.members.push(member("web", "/repo/web/zaz.toml"));
        assert!(sup.check_token_uniqueness().is_ok());
    }

    #[test]
    fn check_token_uniqueness_reports_collision_with_both_paths() {
        let mut sup = test_supervisor();
        sup.members.push(member("dup", "/repo/a/zaz.toml"));
        sup.members.push(member("dup", "/repo/b/zaz.toml"));
        let err = sup.check_token_uniqueness().unwrap_err().to_string();
        assert!(err.contains("dup"));
        assert!(err.contains("/repo/a/zaz.toml"));
        assert!(err.contains("/repo/b/zaz.toml"));
    }

    #[test]
    fn member_by_token_finds_and_misses() {
        let mut sup = test_supervisor();
        sup.members.push(member("api", "/repo/api/zaz.toml"));
        assert!(sup.member_by_token("api").is_some());
        assert!(sup.member_by_token("web").is_none());
    }

    #[test]
    fn combine_fanout_all_success() {
        let response = combine_fanout("config reloaded", 2, 2, Vec::new());
        match response {
            ApiResponse::Ok { message } => {
                assert_eq!(
                    message.as_deref(),
                    Some("config reloaded across 2 project(s)")
                );
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn combine_fanout_partial_failure_is_error() {
        let response = combine_fanout(
            "restart initiated for all groups",
            1,
            2,
            vec!["web: boom".to_string()],
        );
        match response {
            ApiResponse::Error { message } => {
                assert!(message.contains("1/2 project(s) failed"));
                assert!(message.contains("web: boom"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn combine_fanout_zero_members_is_error() {
        let response = combine_fanout("config reloaded", 0, 0, Vec::new());
        assert!(matches!(response, ApiResponse::Error { .. }));
    }

    #[test]
    fn same_config_matches_identical_and_rejects_distinct_paths() {
        // Non-existent paths fall back to literal comparison.
        assert!(same_config(
            Path::new("/repo/a/zaz.toml"),
            Path::new("/repo/a/zaz.toml")
        ));
        assert!(!same_config(
            Path::new("/repo/a/zaz.toml"),
            Path::new("/repo/b/zaz.toml")
        ));
    }

    #[test]
    fn same_config_canonicalizes_before_comparing() {
        let dir = std::env::temp_dir().join("zaz-supervisor-same-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("zaz.toml");
        std::fs::write(&file, "").unwrap();

        // A path with a redundant `.` component canonicalizes to the same file.
        let indirect = dir.join(".").join("zaz.toml");
        assert!(same_config(&file, &indirect));
        // A sibling that does not exist is not the same config.
        assert!(!same_config(&file, &dir.join("other.toml")));
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

    #[tokio::test]
    async fn status_request_answers_running() {
        let sup = test_supervisor();
        let (response, is_shutdown) = sup.handle_request(ApiRequest::Status).await;
        assert!(!is_shutdown);
        match response {
            ApiResponse::Status { state } => assert_eq!(state.status, DaemonStatus::Running),
            other => panic!("expected Status response, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn shutdown_request_signals_teardown() {
        let sup = test_supervisor();
        let (response, is_shutdown) = sup.handle_request(ApiRequest::Shutdown).await;
        assert!(is_shutdown);
        assert!(matches!(response, ApiResponse::Ok { .. }));
    }

    #[tokio::test]
    async fn log_request_without_a_project_is_rejected() {
        let sup = test_supervisor();
        let (response, is_shutdown) = sup
            .handle_request(ApiRequest::GetLogs {
                name: "*".to_string(),
                project: None,
                lines: None,
                offset: None,
                limit: None,
                search: None,
            })
            .await;
        assert!(!is_shutdown);
        match response {
            ApiResponse::Error { message } => assert!(message.contains("requires a project")),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn restart_group_rejects_malformed_and_unknown() {
        let mut sup = test_supervisor();
        sup.members.push(member("api", "/repo/api/zaz.toml"));

        let (malformed, _) = sup
            .handle_request(ApiRequest::RestartGroup {
                name: "bareword".to_string(),
                project: None,
            })
            .await;
        match malformed {
            ApiResponse::Error { message } => assert!(message.contains("malformed qualified name")),
            other => panic!("expected Error, got {:?}", other),
        }

        let (unknown, _) = sup
            .handle_request(ApiRequest::RestartGroup {
                name: "nope/g".to_string(),
                project: None,
            })
            .await;
        match unknown {
            ApiResponse::Error { message } => assert!(message.contains("unknown project 'nope'")),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn resolve_target_prefers_field_then_shorthand() {
        // Structured field wins; the string stays a bare name.
        assert_eq!(
            resolve_target(Some("web".to_string()), "g".to_string()),
            Ok(("web".to_string(), "g".to_string()))
        );
        // No field: parse the `project/group` shorthand.
        assert_eq!(
            resolve_target(None, "web/g".to_string()),
            Ok(("web".to_string(), "g".to_string()))
        );
        // No field and no separator is rejected.
        assert!(resolve_target(None, "bare".to_string()).is_err());
    }

    #[tokio::test]
    async fn aggregate_status_marks_unreachable_members_failed() {
        let mut sup = test_supervisor();
        // Members whose sockets are not bound: each fans out to nothing and
        // surfaces as a Failed marker named after its token.
        sup.members
            .push(member("api", "/tmp/zaz-agg-test/api/zaz.toml"));
        sup.members
            .push(member("web", "/tmp/zaz-agg-test/web/zaz.toml"));

        let (response, _) = sup.handle_request(ApiRequest::Status).await;
        match response {
            ApiResponse::Status { state } => {
                assert_eq!(state.status, DaemonStatus::Running);
                assert_eq!(state.groups.len(), 2);
                for token in ["api", "web"] {
                    let group = state.groups.get(token).expect("marker for token");
                    assert_eq!(group.name, token);
                    assert_eq!(group.status, GroupStatus::Failed);
                    assert!(group.tasks.is_empty() && group.daemons.is_empty());
                }
            }
            other => panic!("expected Status, got {:?}", other),
        }
    }
}
