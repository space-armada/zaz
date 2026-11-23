//! Workspace supervisor: owns an ad-hoc working set of single-config child
//! daemons.
//!
//! The supervisor spawns one ordinary `zaz daemon` per member config (ADR-0007),
//! adopting a member whose hashed socket is already live rather than killing it.
//! The single-config engine is untouched: each member keeps its own socket, so
//! member-directory commands still resolve to the member's own daemon. Control
//! verbs and log queries route to one member by project token; `Status` fans out
//! and merges every reachable member's groups under qualified names.
//!
//! A member whose config does not parse still gets a daemon, under a provisional token taken from
//! its directory name. The daemon reports its load error, and the supervisor re-resolves the real
//! token once the member loads. A member that cannot hold its token is marked failed and leaves
//! routing and the aggregate's group keys, without disturbing the rest of the set. A workspace
//! reload retries every failed member that still has a daemon.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use tokio::sync::{mpsc, oneshot};

use zaz_daemon::{
    socket_path_for_config, ApiRequest, ApiResponse, Client, DaemonState, DaemonStatus,
    EngineCommand, GroupState, GroupStatus, LoadError, MemberError, Server,
};
use zaz_process::LaunchHandle;

use crate::startup::{
    check_daemon_availability, is_daemon_responsive, wait_for_first_reply, DaemonAvailability,
    FirstReply, DAEMON_READY_ATTEMPTS, DAEMON_READY_INTERVAL,
};

/// How long the aggregate waits for one member's `Status`. Control verbs are not bounded.
const MEMBER_STATUS_BOUND: Duration = Duration::from_millis(2500);

/// A member of the working set. `handle` is `Some` only for children the
/// supervisor spawned; an adopted member's daemon predates the supervisor and is
/// left running on teardown.
struct Member {
    config_path: PathBuf,
    socket_path: PathBuf,
    handle: Option<LaunchHandle>,
    adopted: bool,

    /// Whether the supervisor has a daemon for this member. False when attaching it failed, and
    /// after its daemon was shut down because the member lost its token.
    attached: bool,

    /// The addressing label. Empty for a member that was never attached.
    token: String,

    /// Whether `token` was taken from the directory name because the config did not parse. The
    /// supervisor re-resolves it once the member loads.
    provisional: bool,

    /// Why the member is out of the working set. A failed member is excluded from token routing
    /// and from the aggregate's group keys, and shows under its config path.
    failure: Option<LoadError>,
}

impl Member {
    fn failed_to_attach(config_path: PathBuf, socket_path: PathBuf, error: &anyhow::Error) -> Self {
        Self {
            config_path,
            socket_path,
            handle: None,
            adopted: false,
            attached: false,
            token: String::new(),
            provisional: false,
            failure: Some(LoadError::new(
                "workspace_attach_failed",
                format!("{error:#}"),
            )),
        }
    }

    /// Whether requests may be routed to this member by its token.
    fn is_routed(&self) -> bool {
        self.attached && self.failure.is_none()
    }
}

/// A token from the config directory's name, when it has a usable one.
fn basename_token(config_path: &Path) -> Option<String> {
    let basename = config_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());

    match basename {
        Some(name) if !name.is_empty() && !name.contains('/') => Some(name.to_string()),
        _ => None,
    }
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

    match basename_token(config_path) {
        Some(token) => Ok(token),
        None => bail!(
            "cannot derive a project token from {}; set an explicit [settings] name",
            config_path.display()
        ),
    }
}

/// The error for a member that lost a token to another member.
fn collision_error(token: &str, failed: &Path, other: &Path) -> LoadError {
    LoadError::new(
        "workspace_token_collision",
        format!(
            "member {} resolves to project token '{}', which member {} already holds; set an \
             explicit [settings] name in one of them",
            failed.display(),
            token,
            other.display()
        ),
    )
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

/// What one member answered to a forwarded reload.
struct ReloadOutcome {
    config_path: PathBuf,
    label: String,
    result: Result<(), String>,
}

/// A fan-out of reloads that finished, waiting for the supervisor loop to settle the set.
pub(crate) struct ReloadDone {
    response_tx: oneshot::Sender<ApiResponse>,
    outcomes: Vec<ReloadOutcome>,
}

/// Owns the working set of member daemons.
pub(crate) struct Supervisor {
    workspace_socket: PathBuf,
    debug: bool,
    members: Vec<Member>,

    /// Whether the boot attach is still running. The aggregate reports `Starting` until it ends.
    booting: bool,
}

impl Supervisor {
    fn new(workspace_socket: PathBuf, debug: bool) -> Self {
        Self {
            workspace_socket,
            debug,
            members: Vec::new(),
            booting: false,
        }
    }

    fn member_count(&self) -> usize {
        self.members.iter().filter(|m| m.attached).count()
    }

    fn member_by_token(&self, token: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|m| m.is_routed() && m.token == token)
    }

    /// Reject a working set in which two members resolve to the same project
    /// token: the token is the only addressing label, so a collision makes
    /// routing ambiguous. Resolvable by setting an explicit `[settings] name`.
    ///
    /// Only resolved tokens are checked. A collision that involves a provisional token fails the
    /// provisional member instead, in [`Supervisor::fail_provisional_collisions`].
    fn check_token_uniqueness(&self) -> Result<()> {
        let mut seen: HashMap<&str, &Path> = HashMap::new();
        for member in self
            .members
            .iter()
            .filter(|m| m.is_routed() && !m.provisional)
        {
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

    /// Fail every provisional member whose token another member already holds. A resolved member
    /// always keeps its token. Between two provisional members, the one later in config order
    /// fails. Nothing aborts, and the daemons stay up for a fix and a reload.
    fn fail_provisional_collisions(&mut self) {
        let mut taken: HashMap<String, PathBuf> = self
            .members
            .iter()
            .filter(|m| m.is_routed() && !m.provisional)
            .map(|m| (m.token.clone(), m.config_path.clone()))
            .collect();

        for member in self
            .members
            .iter_mut()
            .filter(|m| m.is_routed() && m.provisional)
        {
            match taken.get(&member.token) {
                Some(other) => {
                    let error = collision_error(&member.token, &member.config_path, other);
                    tracing::error!(error = %error.message, "workspace member failed");
                    member.failure = Some(error);
                }
                None => {
                    taken.insert(member.token.clone(), member.config_path.clone());
                }
            }
        }
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

    /// Attach a member: adopt its daemon if the hashed socket is already live,
    /// otherwise spawn one. A config that does not parse still launches, under a
    /// provisional token. A failure returns `Err` without touching the working set.
    async fn attach(&self, config_path: &Path) -> Result<Member> {
        if !config_path.exists() {
            bail!("config file not found: {}", config_path.display());
        }

        let (token, provisional) = match zaz_config::load(config_path) {
            Ok(config) => (
                resolve_token(config_path, config.settings.name.as_deref())?,
                false,
            ),
            Err(load_error) => match basename_token(config_path) {
                Some(token) => {
                    tracing::warn!(
                        config = %config_path.display(),
                        token = %token,
                        error = %load_error,
                        "member config does not load; using a provisional project token"
                    );
                    (token, true)
                }
                None => bail!(
                    "invalid config: {}: {}; no project token can be derived from its directory, \
                     so the member was not launched",
                    config_path.display(),
                    load_error
                ),
            },
        };

        let canonical = config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.to_path_buf());

        let socket = socket_path_for_config(config_path);
        let timeout = Duration::from_secs(1);

        if check_daemon_availability(&socket, timeout)
            .await
            .is_responding()
        {
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
                    return Ok(Member {
                        config_path: canonical,
                        socket_path: socket,
                        handle: None,
                        adopted: true,
                        attached: true,
                        token,
                        provisional,
                        failure: None,
                    });
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

        // The member returns on its first reply and stays up whatever its first load reports.
        // Its load error shows in the aggregate, and a workspace reload retries it.
        match wait_for_first_reply(
            &socket,
            &mut handle,
            DAEMON_READY_ATTEMPTS,
            DAEMON_READY_INTERVAL,
        )
        .await?
        {
            FirstReply::Replied(_) => {
                tracing::info!(
                    config = %config_path.display(),
                    socket = %socket.display(),
                    pid = handle.id(),
                    "spawned member daemon"
                );
                Ok(Member {
                    config_path: canonical,
                    socket_path: socket,
                    handle: Some(handle),
                    adopted: false,
                    attached: true,
                    token,
                    provisional,
                    failure: None,
                })
            }
            FirstReply::Crashed(status) => bail!(
                "member daemon for {} exited before becoming ready (status: {}); see {}",
                config_path.display(),
                status,
                output_log.display()
            ),
            FirstReply::Timeout => {
                let _ = handle.kill();
                bail!(
                    "member daemon for {} did not become ready in time; see {}",
                    config_path.display(),
                    output_log.display()
                )
            }
        }
    }

    /// Attach a member at boot. A failure is recorded as a failed member, so the aggregate names
    /// it, and the rest of the set is undisturbed.
    async fn boot_attach(&mut self, config_path: &Path) {
        let canonical = config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.to_path_buf());
        if self.members.iter().any(|m| m.config_path == canonical) {
            tracing::error!(
                config = %config_path.display(),
                "config already attached"
            );
            return;
        }

        match self.attach(config_path).await {
            Ok(member) => self.members.push(member),
            Err(e) => {
                tracing::error!(
                    config = %config_path.display(),
                    error = %e,
                    "failed to attach workspace member"
                );
                self.members.push(Member::failed_to_attach(
                    canonical,
                    socket_path_for_config(config_path),
                    &e,
                ));
            }
        }
    }

    /// Mark a member failed. With `shutdown_daemon`, also stop its daemon, which has already
    /// loaded and started services. A daemon the supervisor adopted is left running.
    async fn fail_member(&mut self, index: usize, error: LoadError, shutdown_daemon: bool) {
        tracing::error!(
            member = %self.members[index].config_path.display(),
            error = %error.message,
            "workspace member failed"
        );

        let member = &mut self.members[index];
        member.failure = Some(error);

        if !shutdown_daemon || !member.attached || member.adopted {
            return;
        }

        let socket = member.socket_path.clone();
        stop_child(&socket).await;

        let member = &mut self.members[index];
        if let Some(mut handle) = member.handle.take() {
            let _ = handle.kill();
        }
        member.attached = false;
    }

    /// Stop every spawned child; adopted members are dropped without stopping.
    /// A spawned child is stopped cleanly; an adopted member's daemon is left
    /// running, honoring adopt's "don't kill" intent.
    async fn detach_all(&mut self) {
        let members = std::mem::take(&mut self.members);
        for mut member in members {
            if member.adopted || !member.attached {
                continue;
            }

            stop_child(&member.socket_path).await;

            // Hard-reap the child we spawned after the graceful stop. A member
            // under load can be slow to act on the socket shutdown. Once this
            // supervisor exits, an un-reaped child reparents to init and leaks.
            // The handle is present only for spawned members; adopted members
            // are skipped above and left running.
            if let Some(mut handle) = member.handle.take() {
                let _ = handle.kill();
            }
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

    /// Give a member its real token once its daemon has loaded. A member that cannot hold the
    /// token it resolves to is failed, and its daemon is shut down when the supervisor spawned it.
    ///
    /// A provisional member is always tried. With `retry_failed`, so is any attached member that
    /// carries a failure. That is how a daemon the supervisor adopted, and so left running, gets
    /// back into the set once its config names a free token. The periodic pass leaves failed
    /// members alone, since they would only fail again every tick.
    async fn reresolve_tokens(&mut self, retry_failed: bool) {
        for index in 0..self.members.len() {
            let member = &self.members[index];
            let wanted = member.provisional || (retry_failed && member.failure.is_some());
            if !wanted || !member.attached {
                continue;
            }

            let socket = member.socket_path.clone();
            let loaded = matches!(
                check_daemon_availability(&socket, Duration::from_secs(1)).await,
                DaemonAvailability::Responding(state)
                    if state.load_error.is_none() && state.status == DaemonStatus::Running
            );
            if !loaded {
                continue;
            }

            let config_path = self.members[index].config_path.clone();
            let Ok(config) = zaz_config::load(&config_path) else {
                continue;
            };

            match resolve_token(&config_path, config.settings.name.as_deref()) {
                Ok(token) => {
                    tracing::info!(
                        config = %config_path.display(),
                        token = %token,
                        "re-resolved the provisional project token"
                    );

                    let member = &mut self.members[index];
                    member.token = token;
                    member.provisional = false;
                    member.failure = None;

                    self.settle_token(index).await;
                }
                Err(e) => {
                    let error = LoadError::new("workspace_token_invalid", format!("{e:#}"));
                    self.fail_member(index, error, true).await;
                }
            }
        }
    }

    /// Decide whether member `index`, whose token was just resolved, may keep it. A resolved
    /// holder of the same token wins, and the member fails. A provisional holder fails instead.
    async fn settle_token(&mut self, index: usize) {
        let token = self.members[index].token.clone();
        let config_path = self.members[index].config_path.clone();

        let holders: Vec<usize> = (0..self.members.len())
            .filter(|&other| {
                other != index
                    && self.members[other].is_routed()
                    && self.members[other].token == token
            })
            .collect();

        for other in holders {
            let other_path = self.members[other].config_path.clone();

            if self.members[other].provisional {
                let error = collision_error(&token, &other_path, &config_path);
                self.fail_member(other, error, false).await;
            } else {
                let error = collision_error(&token, &config_path, &other_path);
                self.fail_member(index, error, true).await;
                return;
            }
        }
    }

    /// Attach again every member whose daemon is not running, in place so config order holds.
    ///
    /// Returns the outcome for each member it tried.
    async fn reattach_down_members(&mut self) -> Vec<(PathBuf, Result<(), String>)> {
        let mut tried = Vec::new();

        for index in 0..self.members.len() {
            let member = &self.members[index];
            let down = !member.attached
                || !is_daemon_responsive(&member.socket_path, Duration::from_secs(1)).await;
            if !down {
                continue;
            }

            let config_path = self.members[index].config_path.clone();
            if let Some(mut handle) = self.members[index].handle.take() {
                let _ = handle.kill();
            }

            match self.attach(&config_path).await {
                Ok(member) => {
                    self.members[index] = member;
                    self.settle_token(index).await;
                    tried.push((config_path, Ok(())));
                }
                Err(e) => {
                    let socket = self.members[index].socket_path.clone();
                    self.members[index] = Member::failed_to_attach(config_path.clone(), socket, &e);
                    tried.push((config_path, Err(format!("{e:#}"))));
                }
            }
        }

        self.fail_provisional_collisions();
        tried
    }

    /// Map an API request to a response and whether it should trigger shutdown.
    /// Mutations and log queries select a member by project token, carried in the
    /// structured `project` field or, when absent, parsed from a `project/group`
    /// string so the CLI shorthand keeps working. `RestartAll` fans out across the
    /// set, and `Status` merges every reachable member's groups under qualified
    /// names. A reload is not handled here, since it runs off the supervisor loop.
    async fn handle_request(&self, request: ApiRequest) -> (ApiResponse, bool) {
        match request {
            ApiRequest::Status | ApiRequest::ListGroups | ApiRequest::Subscribe => {
                (self.aggregate_status().await, false)
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
                ApiResponse::error("reload is handled by the supervisor loop"),
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
    /// visible without taking the aggregate down.
    ///
    /// The aggregate is `Starting` while the boot attach runs and while any member is, so a
    /// launcher can wait on it. A member whose first load failed is not starting. It carries its
    /// error. A failed member is left out of the group keys and shows under its config path.
    async fn aggregate_status(&self) -> ApiResponse {
        let mut merged = DaemonState::default();
        let mut starting = self.booting;

        // Ask every member at once, each under a bound, so one stuck member cannot hold the
        // aggregate past a launcher's hang budget. A member that misses the bound reads as
        // unreachable.
        let mut probes: Vec<_> = self
            .members
            .iter()
            .map(|member| {
                if member.failure.is_some() {
                    return None;
                }

                let socket = member.socket_path.clone();
                Some(tokio::spawn(async move {
                    match tokio::time::timeout(
                        MEMBER_STATUS_BOUND,
                        forward(&socket, &ApiRequest::Status),
                    )
                    .await
                    {
                        Ok(reply) => reply,
                        Err(_) => Err(format!(
                            "member daemon did not answer within {}s",
                            MEMBER_STATUS_BOUND.as_secs_f32()
                        )),
                    }
                }))
            })
            .collect();

        for (member, probe) in self.members.iter().zip(probes.iter_mut()) {
            if let Some(failure) = &member.failure {
                let path = member.config_path.display().to_string();
                merged.groups.insert(
                    path.clone(),
                    GroupState {
                        name: path.clone(),
                        status: GroupStatus::Failed,
                        tasks: Vec::new(),
                        services: Vec::new(),
                    },
                );
                merged.member_errors.push(MemberError {
                    member: path,
                    error: failure.clone(),
                });
                continue;
            }

            let reply = match probe.take() {
                Some(probe) => probe
                    .await
                    .unwrap_or_else(|e| Err(format!("member status probe failed: {e}"))),
                None => Err("member status was not requested".to_string()),
            };

            match reply {
                Ok(ApiResponse::Status { state }) => {
                    merged.watched_files += state.watched_files;
                    merged.last_change = max_option(merged.last_change, state.last_change);

                    if let Some(error) = state.load_error {
                        merged.groups.insert(
                            member.token.clone(),
                            GroupState {
                                name: member.token.clone(),
                                status: GroupStatus::Failed,
                                tasks: Vec::new(),
                                services: Vec::new(),
                            },
                        );
                        merged.member_errors.push(MemberError {
                            member: member.token.clone(),
                            error,
                        });
                        continue;
                    }

                    starting |= state.status == DaemonStatus::Starting;

                    for (group_name, mut group) in state.groups {
                        let qualified = format!("{}/{}", member.token, group_name);
                        group.name = qualified.clone();
                        merged.groups.insert(qualified, group);
                    }
                }
                other => {
                    let reason = match other {
                        Err(e) => e,
                        Ok(_) => "member daemon sent an unexpected reply".to_string(),
                    };
                    let marker = GroupState {
                        name: member.token.clone(),
                        status: GroupStatus::Failed,
                        tasks: Vec::new(),
                        services: Vec::new(),
                    };
                    merged.groups.insert(member.token.clone(), marker);
                    merged.member_errors.push(MemberError {
                        member: member.token.clone(),
                        error: LoadError::new("member_unreachable", reason),
                    });
                }
            }
        }

        merged.status = if starting {
            DaemonStatus::Starting
        } else {
            DaemonStatus::Running
        };

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

    /// Forward `request` to every routed member, combining the outcomes into one
    /// response. Used by the unqualified `RestartAll` verb.
    async fn fan_out(&self, verb: &str, request: ApiRequest) -> ApiResponse {
        let mut ok = 0usize;
        let mut failures: Vec<String> = Vec::new();
        let mut total = 0usize;
        for member in self.members.iter().filter(|m| m.is_routed()) {
            total += 1;
            match forward(&member.socket_path, &request).await {
                Ok(ApiResponse::Ok { .. }) => ok += 1,
                Ok(ApiResponse::Error { message }) => {
                    failures.push(format!("{}: {}", member.token, message))
                }
                Ok(_) => failures.push(format!("{}: unexpected response", member.token)),
                Err(e) => failures.push(format!("{}: {}", member.token, e)),
            }
        }
        combine_fanout(verb, ok, total, failures)
    }

    /// Forward a reload to every attached member at once, off the supervisor loop.
    ///
    /// A member evaluating a slow source answers its reload only when the evaluation ends, so
    /// forwarding one member at a time inline would hold up the supervisor's own `Status` for the
    /// sum of them all. The answer goes to `done` once every member has replied, and the loop then
    /// settles the set.
    fn begin_reload(
        &self,
        response_tx: oneshot::Sender<ApiResponse>,
        done: &mpsc::UnboundedSender<ReloadDone>,
    ) {
        let targets: Vec<(PathBuf, String, PathBuf)> = self
            .members
            .iter()
            .filter(|m| m.attached)
            .map(|m| {
                let label = if m.token.is_empty() {
                    m.config_path.display().to_string()
                } else {
                    m.token.clone()
                };
                (m.config_path.clone(), label, m.socket_path.clone())
            })
            .collect();
        let done = done.clone();

        tokio::spawn(async move {
            let forwards: Vec<_> = targets
                .into_iter()
                .map(|(config_path, label, socket)| {
                    tokio::spawn(async move {
                        let result = match forward(&socket, &ApiRequest::ReloadConfig).await {
                            Ok(ApiResponse::Ok { .. }) => Ok(()),
                            Ok(ApiResponse::Error { message }) => Err(message),
                            Ok(_) => Err("unexpected response".to_string()),
                            Err(e) => Err(e),
                        };

                        ReloadOutcome {
                            config_path,
                            label,
                            result,
                        }
                    })
                })
                .collect();

            let mut outcomes = Vec::new();
            for forward in forwards {
                if let Ok(outcome) = forward.await {
                    outcomes.push(outcome);
                }
            }

            let _ = done.send(ReloadDone {
                response_tx,
                outcomes,
            });
        });
    }

    /// Settle the set after a reload fan-out: attach again every member whose daemon is not
    /// running, and re-resolve the tokens of members that have loaded. Returns the reload answer.
    async fn finish_reload(&mut self, outcomes: Vec<ReloadOutcome>) -> ApiResponse {
        let tried = self.reattach_down_members().await;
        self.reresolve_tokens(true).await;

        let mut ok = 0usize;
        let mut total = 0usize;
        let mut failures: Vec<String> = Vec::new();

        for outcome in &outcomes {
            total += 1;
            let reattached = tried
                .iter()
                .any(|(path, result)| *path == outcome.config_path && result.is_ok());

            match &outcome.result {
                Ok(()) => ok += 1,
                // A fresh daemon loaded the current config, so the failed forward no longer counts.
                Err(_) if reattached => ok += 1,
                Err(e) => failures.push(format!("{}: {}", outcome.label, e)),
            }
        }

        for (path, result) in &tried {
            if outcomes.iter().any(|o| o.config_path == *path) {
                continue;
            }

            total += 1;
            match result {
                Ok(()) => ok += 1,
                Err(e) => failures.push(format!("{}: {}", path.display(), e)),
            }
        }

        // A member that loaded but lost its token to another member did reload, and is now out of
        // the set. The caller needs to hear that.
        for member in &self.members {
            let Some(failure) = &member.failure else {
                continue;
            };
            let counted_ok = outcomes
                .iter()
                .any(|o| o.config_path == member.config_path && o.result.is_ok())
                || tried
                    .iter()
                    .any(|(path, result)| *path == member.config_path && result.is_ok());

            if counted_ok && failure.code != "workspace_attach_failed" {
                ok = ok.saturating_sub(1);
                failures.push(format!(
                    "{}: [{}] {}",
                    member.config_path.display(),
                    failure.code,
                    failure.message
                ));
            }
        }

        combine_fanout("config reloaded", ok, total, failures)
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

/// Answer every request already queued, without waiting for more. Returns whether one of them
/// was a shutdown.
///
/// The boot attach runs between awaits that cannot be interrupted, so it calls this between
/// members to keep `Status` answering while the set comes up.
async fn answer_queued_requests(
    supervisor: &Supervisor,
    command_rx: &mut mpsc::Receiver<EngineCommand>,
) -> bool {
    while let Ok(command) = command_rx.try_recv() {
        let (response, is_shutdown) = match command.request {
            ApiRequest::ReloadConfig => (
                ApiResponse::error(
                    "workspace is still starting [daemon_starting]: retry the reload shortly",
                ),
                false,
            ),
            request => supervisor.handle_request(request).await,
        };
        let _ = command.response_tx.send(response);

        if is_shutdown {
            return true;
        }
    }

    false
}

/// Run a workspace supervisor in the foreground. Mirrors `run_daemon`: refuse a
/// live workspace socket, bind, attach the members, then serve a control surface
/// (`Status` for readiness, reloads, restarts, and `Shutdown` to tear the set down).
pub(crate) async fn run_supervisor(
    config_paths: &[PathBuf],
    socket_path: &Path,
    debug: bool,
    quiet: bool,
) -> Result<()> {
    let check_timeout = Duration::from_secs(1);
    if check_daemon_availability(socket_path, check_timeout)
        .await
        .is_responding()
    {
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
    supervisor.booting = true;

    // Bind before the members attach, so a launcher gets `Starting` for as long as the set takes.
    let (command_tx, mut command_rx) = mpsc::channel::<EngineCommand>(32);
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let server = Server::bind(socket_path, command_tx, shutdown_tx).await?;
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            tracing::error!(error = %e, "server error");
        }
    });

    // Boot attach loop. A failed member is recorded and the rest of the set is undisturbed.
    let mut shutdown_requested = false;
    for config_path in config_paths {
        if answer_queued_requests(&supervisor, &mut command_rx).await {
            shutdown_requested = true;
            break;
        }

        supervisor.boot_attach(config_path).await;
    }
    tracing::info!(
        attached = supervisor.member_count(),
        "workspace boot attach loop complete"
    );

    // Cross-member token uniqueness. A collision between resolved tokens aborts startup. Stop
    // the children we just spawned first so the failed boot does not leak daemons. A collision
    // that involves a provisional token fails that member instead.
    if !shutdown_requested {
        if let Err(e) = supervisor.check_token_uniqueness() {
            server_handle.abort();
            supervisor.detach_all().await;
            return Err(e);
        }

        supervisor.fail_provisional_collisions();
        supervisor.booting = false;
    }

    let (reload_tx, mut reload_rx) = mpsc::unbounded_channel::<ReloadDone>();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    while !shutdown_requested {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal");
                break;
            }

            Some(cmd) = command_rx.recv() => {
                match cmd.request {
                    ApiRequest::ReloadConfig => supervisor.begin_reload(cmd.response_tx, &reload_tx),
                    request => {
                        let (response, _is_shutdown) = supervisor.handle_request(request).await;
                        let _ = cmd.response_tx.send(response);
                    }
                }
            }

            Some(done) = reload_rx.recv() => {
                let response = supervisor.finish_reload(done.outcomes).await;
                let _ = done.response_tx.send(response);
            }

            // Tear down once a Shutdown response has been written to its client
            _ = shutdown_rx.recv() => {
                shutdown_requested = true;
            }

            _ = tick.tick() => {
                supervisor.reap_children();
                supervisor.reresolve_tokens(false).await;
            }
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
            attached: true,
            token: token.to_string(),
            provisional: false,
            failure: None,
        }
    }

    fn provisional(token: &str, config: &str) -> Member {
        Member {
            provisional: true,
            ..member(token, config)
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
    fn basename_token_needs_a_directory_name() {
        assert_eq!(
            basename_token(Path::new("/repo/api/zaz.toml")),
            Some("api".to_string())
        );
        assert_eq!(basename_token(Path::new("zaz.toml")), None);
        assert_eq!(basename_token(Path::new("/zaz.toml")), None);
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
    fn check_token_uniqueness_ignores_provisional_and_failed_members() {
        let mut sup = test_supervisor();
        sup.members.push(member("dup", "/repo/a/zaz.toml"));
        sup.members.push(provisional("dup", "/repo/b/zaz.toml"));
        assert!(sup.check_token_uniqueness().is_ok());
    }

    #[test]
    fn a_provisional_member_that_collides_with_a_resolved_one_fails() {
        let mut sup = test_supervisor();
        sup.members
            .push(provisional("api", "/repo/broken/api/zaz.toml"));
        sup.members.push(member("api", "/repo/api/zaz.toml"));

        sup.fail_provisional_collisions();

        let failed = sup.members[0].failure.as_ref().expect("provisional fails");
        assert_eq!(failed.code, "workspace_token_collision");
        assert!(failed.message.contains("/repo/broken/api/zaz.toml"));
        assert!(failed.message.contains("/repo/api/zaz.toml"));
        assert!(sup.members[1].failure.is_none());
    }

    #[test]
    fn between_two_provisional_members_the_later_one_fails() {
        let mut sup = test_supervisor();
        sup.members.push(provisional("dup", "/repo/a/dup/zaz.toml"));
        sup.members.push(provisional("dup", "/repo/b/dup/zaz.toml"));

        sup.fail_provisional_collisions();

        assert!(sup.members[0].failure.is_none());
        let failed = sup.members[1].failure.as_ref().expect("later member fails");
        assert!(failed.message.contains("/repo/b/dup/zaz.toml"));
        assert!(failed.message.contains("/repo/a/dup/zaz.toml"));
    }

    #[test]
    fn a_failed_member_is_excluded_from_token_routing() {
        let mut sup = test_supervisor();
        let mut failed = provisional("api", "/repo/broken/api/zaz.toml");
        failed.failure = Some(LoadError::new("workspace_token_collision", "taken"));
        sup.members.push(failed);
        sup.members.push(member("api", "/repo/api/zaz.toml"));

        let routed = sup
            .member_by_token("api")
            .expect("the healthy member routes");

        assert_eq!(routed.config_path, PathBuf::from("/repo/api/zaz.toml"));
    }

    #[tokio::test]
    async fn re_resolving_onto_a_resolved_token_fails_the_member() {
        let mut sup = test_supervisor();
        sup.members.push(member("api", "/repo/api/zaz.toml"));
        sup.members.push(member("api", "/repo/renamed/zaz.toml"));

        sup.settle_token(1).await;

        assert!(sup.members[0].failure.is_none());
        let failed = sup.members[1].failure.as_ref().expect("the newcomer fails");
        assert_eq!(failed.code, "workspace_token_collision");
    }

    #[tokio::test]
    async fn re_resolving_onto_a_provisional_token_fails_the_provisional_member() {
        let mut sup = test_supervisor();
        sup.members
            .push(provisional("api", "/repo/broken/api/zaz.toml"));
        sup.members.push(member("api", "/repo/renamed/zaz.toml"));

        sup.settle_token(1).await;

        assert!(sup.members[0].failure.is_some());
        assert!(sup.members[1].failure.is_none());
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
    async fn status_reports_starting_while_the_boot_attach_runs() {
        let mut sup = test_supervisor();
        sup.booting = true;

        for request in [ApiRequest::Status, ApiRequest::Subscribe] {
            let (response, _) = sup.handle_request(request).await;
            match response {
                ApiResponse::Status { state } => assert_eq!(state.status, DaemonStatus::Starting),
                other => panic!("expected Status response, got {:?}", other),
            }
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
                    assert!(group.tasks.is_empty() && group.services.is_empty());
                }
                assert_eq!(state.member_errors.len(), 2);
                assert_eq!(state.member_errors[0].error.code, "member_unreachable");
            }
            other => panic!("expected Status, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn one_stuck_member_does_not_stall_the_aggregate() {
        let temp = tempfile::tempdir().unwrap();
        let mut sup = test_supervisor();

        // Two sockets that accept and never answer. Asked one at a time they would take twice
        // the bound.
        let mut held = Vec::new();
        for token in ["slow-a", "slow-b"] {
            let socket = temp.path().join(format!("{token}.sock"));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            held.push(tokio::spawn(async move {
                let mut streams = Vec::new();
                while let Ok((stream, _)) = listener.accept().await {
                    streams.push(stream);
                }
            }));

            let mut stuck = member(token, &format!("/repo/{token}/zaz.toml"));
            stuck.socket_path = socket;
            sup.members.push(stuck);
        }

        let begun = Instant::now();
        let (response, _) = sup.handle_request(ApiRequest::Status).await;
        let took = begun.elapsed();

        let ApiResponse::Status { state } = response else {
            panic!("expected Status");
        };
        assert!(took < Duration::from_millis(4500), "{took:?}");
        assert_eq!(state.member_errors.len(), 2);
        assert_eq!(state.member_errors[0].error.code, "member_unreachable");
        assert!(state.member_errors[0]
            .error
            .message
            .contains("did not answer"));

        for task in held {
            task.abort();
        }
    }

    #[tokio::test]
    async fn aggregate_status_shows_a_failed_member_under_its_config_path() {
        let mut sup = test_supervisor();
        let mut failed = provisional("api", "/repo/broken/api/zaz.toml");
        failed.failure = Some(collision_error(
            "api",
            Path::new("/repo/broken/api/zaz.toml"),
            Path::new("/repo/api/zaz.toml"),
        ));
        sup.members.push(failed);
        sup.members.push(Member::failed_to_attach(
            PathBuf::from("/repo/missing/zaz.toml"),
            PathBuf::from("/tmp/missing.sock"),
            &anyhow::anyhow!("config file not found: /repo/missing/zaz.toml"),
        ));

        let (response, _) = sup.handle_request(ApiRequest::Status).await;
        let ApiResponse::Status { state } = response else {
            panic!("expected Status");
        };

        assert_eq!(state.status, DaemonStatus::Running);
        assert!(!state.groups.contains_key("api"));
        assert_eq!(
            state.groups["/repo/broken/api/zaz.toml"].status,
            GroupStatus::Failed
        );
        assert_eq!(state.member_errors.len(), 2);
        assert_eq!(state.member_errors[0].member, "/repo/broken/api/zaz.toml");
        assert_eq!(
            state.member_errors[0].error.code,
            "workspace_token_collision"
        );
        assert_eq!(state.member_errors[1].error.code, "workspace_attach_failed");
    }
}
