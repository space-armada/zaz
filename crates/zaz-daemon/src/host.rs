//! What the daemon's event loop owns: a typed unloaded-or-loaded state, and the reload queue.
//!
//! The daemon binds its socket knowing only its config path. Loading and evaluating the config
//! runs on a separate thread, so the loop keeps answering requests however long a source takes.
//! A config that fails to load leaves the host unloaded, with the failure recorded, rather than
//! half-built. Recovery runs the whole engine constructor on a fresh load.
//!
//! At most one load is in flight and at most one is pending. A reload that arrives while one is
//! in flight joins the pending run. Each waiter gets the result of the run it joined, and runs
//! apply in the order they started. Nothing is cancelled except by shutdown.
//!
//! A run that outlasts its deadline is abandoned. The thread stuck in a read cannot be killed, so
//! it is left behind and its result is dropped. The run fails with its own error, and the pending
//! run, if there is one, starts.

use crate::api::{ApiRequest, ApiResponse, EngineCommand};
use crate::engine::{answer_log_request, Engine};
use crate::evaluate::{self, Prepared};
use crate::log_store::SharedLogStore;
use crate::state::{DaemonState, DaemonStatus, LoadError};
use crate::DaemonError;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

type Waiter = oneshot::Sender<ApiResponse>;

/// How much of the config a run reads.
///
/// Only runs that read the whole config exist so far. A run that reads sources alone joins them
/// later, and a request that joins a pending narrower run will widen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunScope {
    Full,
}

enum Slot {
    /// No engine is built. `error` is why the first load failed, once one has.
    Unloaded {
        error: Option<LoadError>,
    },

    Loaded(Box<Engine>),
}

struct InFlight {
    id: u64,
    waiters: Vec<Waiter>,
}

struct Pending {
    scope: RunScope,
    waiters: Vec<Waiter>,
}

/// The result of one load and evaluation, on its way back to the event loop.
pub struct RunOutcome {
    id: u64,
    result: Result<Prepared, DaemonError>,
}

/// The daemon's event-loop state.
pub struct DaemonHost {
    config_path: PathBuf,
    verbose_output: bool,
    slot: Slot,
    in_flight: Option<InFlight>,
    pending: Option<Pending>,
    next_run: u64,
    outcomes_tx: mpsc::UnboundedSender<RunOutcome>,
    read_deadline: Duration,
    shutting_down: bool,
}

impl DaemonHost {
    /// Create a host for the config at `config_path`.
    ///
    /// Nothing is read. The returned receiver yields each run's outcome, and the event loop passes
    /// every one to [`DaemonHost::finish_run`]. Call [`DaemonHost::begin_first_load`] once the
    /// socket is bound.
    pub fn new(
        config_path: &Path,
        verbose_output: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<RunOutcome>), DaemonError> {
        let config_path =
            zaz_config::absolute_config_path(config_path).map_err(DaemonError::Config)?;
        let (outcomes_tx, outcomes_rx) = mpsc::unbounded_channel();

        let host = Self {
            config_path,
            verbose_output,
            slot: Slot::Unloaded { error: None },
            in_flight: None,
            pending: None,
            next_run: 0,
            outcomes_tx,
            read_deadline: evaluate::ENV_FILE_READ_DEADLINE,
            shutting_down: false,
        };

        Ok((host, outcomes_rx))
    }

    /// Shorten the per-read deadline so a test does not wait out the real one.
    #[cfg(test)]
    fn with_read_deadline(mut self, deadline: Duration) -> Self {
        self.read_deadline = deadline;
        self
    }

    /// Start the first load. The first load is the first run, so a reload that arrives before it
    /// finishes joins the pending run.
    pub fn begin_first_load(&mut self) {
        tracing::info!(config = %self.config_path.display(), "loading configuration");
        self.request_run(RunScope::Full, None);
    }

    /// Whether the config loaded and the engine is built.
    pub fn is_loaded(&self) -> bool {
        matches!(self.slot, Slot::Loaded(_))
    }

    /// The state a status request reports.
    pub fn state(&mut self) -> DaemonState {
        match &mut self.slot {
            Slot::Loaded(engine) => engine.status_snapshot(),
            Slot::Unloaded { error } => DaemonState {
                status: DaemonStatus::Starting,
                load_error: error.clone(),
                ..Default::default()
            },
        }
    }

    /// Answer one API request.
    ///
    /// A reload is answered when its run finishes, and everything else at once.
    pub async fn handle_command(&mut self, command: EngineCommand) {
        let EngineCommand {
            request,
            response_tx,
        } = command;

        match request {
            ApiRequest::ReloadConfig => self.request_reload(response_tx),
            ApiRequest::Shutdown => {
                let response = self.begin_shutdown();
                let _ = response_tx.send(response);
            }
            other => {
                let response = self.answer(other).await;
                let _ = response_tx.send(response);
            }
        }
    }

    /// Apply the outcome of a run, then start the pending run if there is one.
    ///
    /// An outcome that arrives after shutdown, or from a run that is no longer in flight, is
    /// discarded.
    ///
    /// Building and starting services can take as long as their start delays add up to. The
    /// loop's own `commands` keep being answered meanwhile, from the state as it stood when the
    /// apply began, so a launcher never sees silence from a daemon that is working.
    pub async fn finish_run(
        &mut self,
        outcome: RunOutcome,
        commands: &mut mpsc::Receiver<EngineCommand>,
    ) {
        if self.shutting_down {
            tracing::debug!("discarding a load that finished after shutdown");
            return;
        }

        // The run stays in flight while it is applied, so a reload that arrives meanwhile joins
        // the pending run instead of starting an evaluation of its own.
        let waiters = match self.in_flight.as_mut() {
            Some(in_flight) if in_flight.id == outcome.id => std::mem::take(&mut in_flight.waiters),
            _ => return,
        };

        let response = self.apply(outcome.result, commands).await;
        self.in_flight = None;

        let response = if self.shutting_down {
            shutting_down_response()
        } else {
            response
        };
        for waiter in waiters {
            let _ = waiter.send(response.clone());
        }

        if self.shutting_down {
            return;
        }

        if let Some(pending) = self.pending.take() {
            self.start_run(pending.scope, pending.waiters);
        }
    }

    /// Do one pass of background work: drain process output, poll for file changes, and check
    /// services. There is nothing to do until the config loads.
    pub async fn tick(&mut self) {
        let Slot::Loaded(engine) = &mut self.slot else {
            return;
        };

        if let Err(e) = engine.process_incoming_logs() {
            tracing::error!(error = %e, "log drain error");
        }
        if let Err(e) = engine.maybe_enforce_log_retention() {
            tracing::error!(error = %e, "log retention error");
        }
        if let Err(e) = engine.poll().await {
            tracing::error!(error = %e, "poll error");
        }
        if let Err(e) = engine.check_services().await {
            tracing::error!(error = %e, "service check error");
        }
    }

    /// Stop every process the engine started. A load still in flight is abandoned.
    pub async fn shutdown(&mut self) -> Result<(), DaemonError> {
        self.shutting_down = true;

        match &mut self.slot {
            Slot::Loaded(engine) => engine.shutdown().await,
            Slot::Unloaded { .. } => Ok(()),
        }
    }

    fn request_reload(&mut self, waiter: Waiter) {
        if self.shutting_down {
            let _ = waiter.send(shutting_down_response());
            return;
        }

        self.request_run(RunScope::Full, Some(waiter));
    }

    /// Run now when nothing is in flight, otherwise join the pending run.
    fn request_run(&mut self, scope: RunScope, waiter: Option<Waiter>) {
        if self.in_flight.is_none() {
            self.start_run(scope, waiter.into_iter().collect());
            return;
        }

        let pending = self.pending.get_or_insert_with(|| Pending {
            scope,
            waiters: Vec::new(),
        });
        pending.waiters.extend(waiter);
    }

    fn start_run(&mut self, scope: RunScope, waiters: Vec<Waiter>) {
        let id = self.next_run;
        self.next_run += 1;

        tracing::debug!(run = id, scope = ?scope, "starting a load");
        self.in_flight = Some(InFlight { id, waiters });

        let outcomes_tx = self.outcomes_tx.clone();
        evaluate::prepare_detached(
            self.config_path.clone(),
            self.read_deadline,
            move |result| {
                let _ = outcomes_tx.send(RunOutcome { id, result });
            },
        );
    }

    /// Answer every waiter with a shutting-down result, and drop the load in flight.
    fn begin_shutdown(&mut self) -> ApiResponse {
        self.shutting_down = true;

        let in_flight = self.in_flight.take().into_iter().flat_map(|f| f.waiters);
        let pending = self.pending.take().into_iter().flat_map(|p| p.waiters);
        for waiter in in_flight.chain(pending) {
            let _ = waiter.send(shutting_down_response());
        }

        ApiResponse::ok_with_message("shutting down")
    }

    /// Apply a finished run to whatever the host holds, and say how it went.
    async fn apply(
        &mut self,
        result: Result<Prepared, DaemonError>,
        commands: &mut mpsc::Receiver<EngineCommand>,
    ) -> ApiResponse {
        match (
            std::mem::replace(&mut self.slot, Slot::Unloaded { error: None }),
            result,
        ) {
            (Slot::Loaded(mut engine), Ok(prepared)) => {
                let snapshot = engine.status_snapshot();
                let logs = engine.log_handle();
                let applied = self
                    .while_answering(engine.apply_prepared(prepared), &snapshot, &logs, commands)
                    .await;
                self.slot = Slot::Loaded(engine);

                match applied {
                    Some(result) => Engine::reload_response(result),
                    None => shutting_down_response(),
                }
            }
            (Slot::Loaded(engine), Err(e)) => {
                // A failed reload leaves the running config in place and is never a load error.
                self.slot = Slot::Loaded(engine);
                Engine::reload_response(Engine::prepare_failure(&e))
            }
            (Slot::Unloaded { .. }, Ok(prepared)) => self.load_first(prepared, commands).await,
            (Slot::Unloaded { .. }, Err(e)) => {
                tracing::error!(error = %e, "failed to load configuration");
                self.slot = Slot::Unloaded {
                    error: Some(e.to_load_error()),
                };

                ApiResponse::error(format!("reload failed: {}", e))
            }
        }
    }

    /// Drive `work` to the end while the loop's `commands` keep being answered.
    ///
    /// `work` holds the engine, so requests are answered from `snapshot` and from the `logs`
    /// handle rather than from it. A shutdown request ends the wait early. `work` is then dropped
    /// where it stands, and the engine's own shutdown stops whatever it had started. Returns
    /// `None` in that case.
    async fn while_answering<T>(
        &mut self,
        work: impl std::future::Future<Output = T>,
        snapshot: &DaemonState,
        logs: &SharedLogStore,
        commands: &mut mpsc::Receiver<EngineCommand>,
    ) -> Option<T> {
        tokio::pin!(work);

        loop {
            tokio::select! {
                output = &mut work => return Some(output),
                Some(command) = commands.recv() => {
                    self.answer_busy(command, snapshot, logs);

                    if self.shutting_down {
                        return None;
                    }
                }
            }
        }
    }

    /// Answer a request while the engine is busy starting or reloading services.
    ///
    /// The state reads `Running` from the moment the groups are built, which is what the readiness
    /// contract promises. Service starts that wait out a `delay` finish later, and show in the
    /// group states as they do.
    ///
    /// Log requests read the shared log store, so they answer in full during an apply. Restarts
    /// are refused, since the engine that would run them is busy.
    fn answer_busy(
        &mut self,
        command: EngineCommand,
        snapshot: &DaemonState,
        logs: &SharedLogStore,
    ) {
        let EngineCommand {
            request,
            response_tx,
        } = command;

        let response = match request {
            ApiRequest::ReloadConfig => return self.request_reload(response_tx),
            ApiRequest::Shutdown => self.begin_shutdown(),
            ApiRequest::Status | ApiRequest::ListGroups | ApiRequest::Subscribe => {
                ApiResponse::Status {
                    state: snapshot.clone(),
                }
            }
            ApiRequest::RestartGroup { .. }
            | ApiRequest::RestartProcess { .. }
            | ApiRequest::RestartAll => ApiResponse::error(
                "daemon busy [daemon_busy]: it is still starting or reloading its services",
            ),
            request @ (ApiRequest::GetLogs { .. } | ApiRequest::SubscribeLogs { .. }) => {
                if let Err(e) = logs.drain() {
                    tracing::error!(error = %e, "log drain error");
                }

                answer_log_request(logs, request)
            }
            other => answer_unloaded(other, None, &self.config_path),
        };

        let _ = response_tx.send(response);
    }

    /// Build the engine from the first successful load, through the whole constructor.
    async fn load_first(
        &mut self,
        prepared: Prepared,
        commands: &mut mpsc::Receiver<EngineCommand>,
    ) -> ApiResponse {
        let built = Engine::from_prepared(
            prepared,
            self.config_path.clone(),
            self.verbose_output,
            false,
        );

        let mut engine = match built {
            Ok(engine) => engine,
            Err(e) => {
                tracing::error!(error = %e, "failed to build the engine");
                self.slot = Slot::Unloaded {
                    error: Some(e.to_load_error()),
                };

                return ApiResponse::error(format!("reload failed: {}", e));
            }
        };

        // The groups are built, so the daemon is `Running` from here, however long the service
        // starts take.
        let mut snapshot = engine.status_snapshot();
        snapshot.status = DaemonStatus::Running;
        let groups = snapshot.groups.len();

        // A failed startup keeps the daemon up, as it always has.
        let logs = engine.log_handle();
        let started = self
            .while_answering(engine.startup(), &snapshot, &logs, commands)
            .await;
        if let Some(Err(e)) = &started {
            tracing::error!(error = %e, "initial startup failed, waiting for file changes to retry");
        }

        self.slot = Slot::Loaded(Box::new(engine));
        if started.is_none() {
            return shutting_down_response();
        }

        ApiResponse::ok_with_message(format!(
            "config reloaded: {} added, 0 removed, 0 modified",
            groups
        ))
    }

    async fn answer(&mut self, request: ApiRequest) -> ApiResponse {
        match &mut self.slot {
            Slot::Loaded(engine) => {
                // Drain output first so a log query sees fresh data.
                if let Err(e) = engine.process_incoming_logs() {
                    tracing::error!(error = %e, "log drain error");
                }

                engine.handle_request(request).await
            }
            Slot::Unloaded { error } => answer_unloaded(request, error.as_ref(), &self.config_path),
        }
    }
}

fn shutting_down_response() -> ApiResponse {
    ApiResponse::error("daemon is shutting down [shutting_down]")
}

/// Answer a request while no engine is built. Requests that read state answer with what there
/// is, and requests that act on groups say why they cannot.
fn answer_unloaded(
    request: ApiRequest,
    error: Option<&LoadError>,
    config_path: &Path,
) -> ApiResponse {
    match request {
        ApiRequest::Status | ApiRequest::ListGroups | ApiRequest::Subscribe => {
            ApiResponse::Status {
                state: DaemonState {
                    status: DaemonStatus::Starting,
                    load_error: error.cloned(),
                    ..Default::default()
                },
            }
        }
        ApiRequest::GetLogs { name, .. } | ApiRequest::SubscribeLogs { name } => {
            ApiResponse::Logs {
                name,
                lines: Vec::new(),
                total_count: None,
                has_more: None,
                offset: None,
            }
        }
        ApiRequest::Identify => {
            let config_path = config_path
                .canonicalize()
                .unwrap_or_else(|_| config_path.to_path_buf());

            ApiResponse::Identity {
                config_path: config_path.display().to_string(),
            }
        }
        ApiRequest::RestartGroup { .. }
        | ApiRequest::RestartProcess { .. }
        | ApiRequest::RestartAll => match error {
            Some(error) => ApiResponse::error(format!(
                "first load failed [first_load_failed], fix the config and run `zaz reload`: {}",
                error.message
            )),
            None => ApiResponse::error(
                "daemon starting [daemon_starting]: the daemon is still loading its configuration",
            ),
        },
        ApiRequest::ReloadConfig | ApiRequest::Shutdown => {
            ApiResponse::error("request is handled by the daemon host")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const CONFIG: &str = "[[group]]\nname = \"app\"\nenv_file = \"slow.env\"\n\n\
                          [[group.task]]\nname = \"noop\"\ncommand = \"true\"\n";

    struct Fixture {
        dir: tempfile::TempDir,
        host: DaemonHost,
        outcomes: mpsc::UnboundedReceiver<RunOutcome>,
        commands: mpsc::Receiver<EngineCommand>,
    }

    impl Fixture {
        /// A config whose env_file is a named pipe, so a load blocks until the test feeds it.
        fn with_fifo() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("zaz.toml"), CONFIG).unwrap();

            let status = std::process::Command::new("mkfifo")
                .arg(dir.path().join("slow.env"))
                .status()
                .unwrap();
            assert!(status.success(), "mkfifo is required for these tests");

            Self::at(dir)
        }

        fn at(dir: tempfile::TempDir) -> Self {
            let (host, outcomes) = DaemonHost::new(&dir.path().join("zaz.toml"), false).unwrap();

            let (_commands_tx, commands) = mpsc::channel(8);

            Self {
                dir,
                host,
                outcomes,
                commands,
            }
        }

        fn with_deadline(mut self, deadline: Duration) -> Self {
            self.host = self.host.with_read_deadline(deadline);
            self
        }

        /// Let the load that is blocked on the pipe read `text`.
        fn feed(&self, text: &'static str) {
            let path = self.dir.path().join("slow.env");

            std::thread::spawn(move || {
                let _ = std::fs::write(path, text);
            });
        }

        async fn next_outcome(&mut self) {
            let outcome = tokio::time::timeout(Duration::from_secs(10), self.outcomes.recv())
                .await
                .expect("a load should finish")
                .expect("the outcome channel stays open");

            self.host.finish_run(outcome, &mut self.commands).await;
        }

        async fn request(&mut self, request: ApiRequest) -> ApiResponse {
            let (response_tx, response_rx) = oneshot::channel();
            self.host
                .handle_command(EngineCommand {
                    request,
                    response_tx,
                })
                .await;

            response_rx.await.unwrap()
        }

        /// Send a reload without waiting for its answer.
        async fn reload(&mut self) -> oneshot::Receiver<ApiResponse> {
            let (response_tx, response_rx) = oneshot::channel();
            self.host
                .handle_command(EngineCommand {
                    request: ApiRequest::ReloadConfig,
                    response_tx,
                })
                .await;

            response_rx
        }
    }

    fn message(response: &ApiResponse) -> &str {
        match response {
            ApiResponse::Ok { message } => message.as_deref().unwrap_or(""),
            ApiResponse::Error { message } => message,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_host_answers_status_while_the_first_load_is_slow() {
        let mut fixture = Fixture::with_fifo();
        fixture.host.begin_first_load();

        let response = fixture.request(ApiRequest::Status).await;
        let ApiResponse::Status { state } = response else {
            panic!("expected a status");
        };
        assert_eq!(state.status, DaemonStatus::Starting);
        assert!(state.load_error.is_none());
        assert!(state.groups.is_empty());

        fixture.feed("A=1\n");
        fixture.next_outcome().await;

        let ApiResponse::Status { state } = fixture.request(ApiRequest::Status).await else {
            panic!("expected a status");
        };
        assert_eq!(state.status, DaemonStatus::Running);
        assert!(state.groups.contains_key("app"));

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn requests_during_starting_answer_or_say_why_not() {
        let mut fixture = Fixture::with_fifo();
        fixture.host.begin_first_load();

        let identify = fixture.request(ApiRequest::Identify).await;
        assert!(matches!(identify, ApiResponse::Identity { .. }));

        let logs = fixture
            .request(ApiRequest::GetLogs {
                name: "*".to_string(),
                project: None,
                lines: None,
                offset: None,
                limit: None,
                search: None,
            })
            .await;
        assert!(matches!(logs, ApiResponse::Logs { ref lines, .. } if lines.is_empty()));

        let subscribe_logs = fixture
            .request(ApiRequest::SubscribeLogs {
                name: "*".to_string(),
            })
            .await;
        assert!(matches!(subscribe_logs, ApiResponse::Logs { .. }));

        let subscribe = fixture.request(ApiRequest::Subscribe).await;
        assert!(matches!(subscribe, ApiResponse::Status { .. }));

        for request in [
            ApiRequest::RestartAll,
            ApiRequest::RestartGroup {
                name: "app".to_string(),
                project: None,
            },
            ApiRequest::RestartProcess {
                group: "app".to_string(),
                process: "noop".to_string(),
                project: None,
            },
        ] {
            let response = fixture.request(request).await;
            assert!(matches!(response, ApiResponse::Error { .. }));
            assert!(
                message(&response).contains("daemon_starting"),
                "{response:?}"
            );
        }

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_first_load_records_a_load_error_and_recovers_on_reload() {
        let mut fixture = Fixture::with_fifo();
        std::fs::remove_file(fixture.dir.path().join("slow.env")).unwrap();
        fixture.host.begin_first_load();
        fixture.next_outcome().await;

        let ApiResponse::Status { state } = fixture.request(ApiRequest::Status).await else {
            panic!("expected a status");
        };
        assert_eq!(state.status, DaemonStatus::Starting);
        assert_eq!(state.load_error.unwrap().code, "env_file_missing");

        let restart = fixture.request(ApiRequest::RestartAll).await;
        assert!(
            message(&restart).contains("first_load_failed"),
            "{restart:?}"
        );

        std::fs::write(fixture.dir.path().join("slow.env"), "A=1\n").unwrap();
        let response = fixture.reload().await;
        fixture.next_outcome().await;

        let response = response.await.unwrap();
        assert_eq!(
            message(&response),
            "config reloaded: 1 added, 0 removed, 0 modified"
        );

        let ApiResponse::Status { state } = fixture.request(ApiRequest::Status).await else {
            panic!("expected a status");
        };
        assert_eq!(state.status, DaemonStatus::Running);
        assert!(state.load_error.is_none());

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn recovery_reads_zaz_toml_again_and_runs_the_whole_constructor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zaz.toml"), "[[group\nname = ").unwrap();
        let mut fixture = Fixture::at(dir);
        fixture.host.begin_first_load();
        fixture.next_outcome().await;

        let ApiResponse::Status { state } = fixture.request(ApiRequest::Status).await else {
            panic!("expected a status");
        };
        let error = state.load_error.expect("a parse failure is a load error");
        assert_eq!(error.code, "parse_error");

        std::fs::write(
            fixture.dir.path().join("zaz.toml"),
            "[[group]]\nname = \"edited\"\npatterns = [\"*.txt\"]\n\n\
             [[group.task]]\nname = \"noop\"\ncommand = \"true\"\n",
        )
        .unwrap();
        let response = fixture.reload().await;
        fixture.next_outcome().await;
        assert!(message(&response.await.unwrap()).starts_with("config reloaded"));

        let ApiResponse::Status { state } = fixture.request(ApiRequest::Status).await else {
            panic!("expected a status");
        };
        assert!(state.load_error.is_none());
        assert!(state.groups.contains_key("edited"));

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_reload_failure_never_sets_load_error() {
        let mut fixture = Fixture::with_fifo();
        fixture.host.begin_first_load();
        fixture.feed("A=1\n");
        fixture.next_outcome().await;
        assert!(fixture.host.is_loaded());

        std::fs::remove_file(fixture.dir.path().join("slow.env")).unwrap();
        let response = fixture.reload().await;
        fixture.next_outcome().await;

        let response = response.await.unwrap();
        assert!(matches!(response, ApiResponse::Error { .. }));
        assert!(
            message(&response).contains("env_file_missing"),
            "{response:?}"
        );

        let state = fixture.host.state();
        assert_eq!(state.status, DaemonStatus::Running);
        assert!(state.load_error.is_none());
        assert!(state.groups.contains_key("app"));

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn overlapping_reloads_coalesce_into_one_pending_run() {
        let mut fixture = Fixture::with_fifo();
        fixture.host.begin_first_load();

        let first = fixture.reload().await;
        let second = fixture.reload().await;
        let third = fixture.reload().await;

        fixture.feed("A=1\n");
        fixture.next_outcome().await;
        assert!(fixture.host.is_loaded());

        fixture.feed("A=2\n");
        fixture.next_outcome().await;

        let first = first.await.unwrap();
        assert_eq!(
            message(&first),
            "config reloaded: 0 added, 0 removed, 1 modified"
        );
        assert_eq!(message(&second.await.unwrap()), message(&first));
        assert_eq!(message(&third.await.unwrap()), message(&first));

        // Two loads ran for the three requests, so there is no third to wait for.
        assert!(fixture.outcomes.try_recv().is_err());
        assert!(fixture.host.pending.is_none());
        assert!(fixture.host.in_flight.is_none());

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_reload_after_a_run_started_gets_the_next_runs_result() {
        let mut fixture = Fixture::with_fifo();
        fixture.host.begin_first_load();
        fixture.feed("A=1\n");
        fixture.next_outcome().await;

        let before = fixture.reload().await;
        let during = fixture.reload().await;

        fixture.feed("A=1\n");
        fixture.next_outcome().await;
        assert_eq!(
            message(&before.await.unwrap()),
            "config reloaded: 0 added, 0 removed, 0 modified"
        );
        assert!(fixture.host.pending.is_none());
        assert!(fixture.host.in_flight.is_some());

        fixture.feed("A=2\n");
        fixture.next_outcome().await;
        assert_eq!(
            message(&during.await.unwrap()),
            "config reloaded: 0 added, 0 removed, 1 modified"
        );

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_answers_every_waiter_and_discards_the_late_result() {
        let mut fixture = Fixture::with_fifo();
        fixture.host.begin_first_load();

        let in_flight_waiter = fixture.reload().await;
        let pending_waiter = fixture.reload().await;

        let response = fixture.request(ApiRequest::Shutdown).await;
        assert_eq!(message(&response), "shutting down");

        for waiter in [in_flight_waiter, pending_waiter] {
            let response = waiter.await.unwrap();
            assert!(message(&response).contains("shutting_down"), "{response:?}");
        }

        let late = fixture.reload().await.await.unwrap();
        assert!(message(&late).contains("shutting_down"), "{late:?}");

        fixture.feed("A=1\n");
        fixture.next_outcome().await;
        assert!(!fixture.host.is_loaded());
    }

    const DELAYED_CONFIG: &str = "[[group]]\nname = \"svc\"\n\n\
                                  [[group.service]]\nname = \"s\"\ncommand = \"sleep 30\"\ndelay = \"3s\"\n";

    /// Send `request` on `commands_tx` after `wait`, and report when and how it was answered.
    async fn probe(
        commands_tx: &mpsc::Sender<EngineCommand>,
        wait: Duration,
        request: ApiRequest,
    ) -> (Duration, ApiResponse) {
        let begun = std::time::Instant::now();
        tokio::time::sleep(wait).await;

        let (response_tx, response_rx) = oneshot::channel();
        commands_tx
            .send(EngineCommand {
                request,
                response_tx,
            })
            .await
            .unwrap();

        let response = response_rx.await.unwrap();
        (begun.elapsed(), response)
    }

    #[tokio::test]
    async fn status_is_answered_while_a_service_start_delay_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zaz.toml"), DELAYED_CONFIG).unwrap();
        let mut fixture = Fixture::at(dir);
        fixture.host.begin_first_load();
        let outcome = fixture.outcomes.recv().await.unwrap();

        let (commands_tx, mut commands) = mpsc::channel(8);
        let begun = std::time::Instant::now();
        let ((), (answered_after, reply)) = tokio::join!(
            fixture.host.finish_run(outcome, &mut commands),
            probe(&commands_tx, Duration::from_millis(300), ApiRequest::Status),
        );

        assert!(
            answered_after < Duration::from_millis(1500),
            "{answered_after:?}"
        );
        assert!(
            begun.elapsed() >= Duration::from_millis(2500),
            "the delay should have run"
        );

        let ApiResponse::Status { state } = reply else {
            panic!("expected a status, got {reply:?}");
        };
        assert_eq!(state.status, DaemonStatus::Running);
        assert!(state.groups.contains_key("svc"));
        assert!(fixture.host.is_loaded());

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restarts_during_a_delayed_start_are_refused_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zaz.toml"), DELAYED_CONFIG).unwrap();
        let mut fixture = Fixture::at(dir);
        fixture.host.begin_first_load();
        let outcome = fixture.outcomes.recv().await.unwrap();

        let (commands_tx, mut commands) = mpsc::channel(8);
        let ((), (_, reply)) = tokio::join!(
            fixture.host.finish_run(outcome, &mut commands),
            probe(
                &commands_tx,
                Duration::from_millis(300),
                ApiRequest::RestartAll
            ),
        );

        assert!(message(&reply).contains("daemon_busy"), "{reply:?}");

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_during_a_delayed_start_ends_the_wait_and_stops_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zaz.toml"), DELAYED_CONFIG).unwrap();
        let mut fixture = Fixture::at(dir);
        fixture.host.begin_first_load();
        let outcome = fixture.outcomes.recv().await.unwrap();

        let (commands_tx, mut commands) = mpsc::channel(8);
        let begun = std::time::Instant::now();
        let ((), (_, reply)) = tokio::join!(
            fixture.host.finish_run(outcome, &mut commands),
            probe(
                &commands_tx,
                Duration::from_millis(300),
                ApiRequest::Shutdown
            ),
        );

        assert_eq!(message(&reply), "shutting down");
        assert!(
            begun.elapsed() < Duration::from_millis(2500),
            "the wait should end early"
        );
        assert!(fixture.host.is_loaded());

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_reload_during_a_delayed_apply_joins_the_pending_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zaz.toml"), CONFIG).unwrap();
        std::fs::write(dir.path().join("slow.env"), "A=1\n").unwrap();
        let mut fixture = Fixture::at(dir);
        fixture.host.begin_first_load();
        fixture.next_outcome().await;
        assert!(fixture.host.is_loaded());

        // A new delayed service makes the reload's apply wait out its delay.
        std::fs::write(
            fixture.dir.path().join("zaz.toml"),
            format!("{CONFIG}{}", DELAYED_CONFIG),
        )
        .unwrap();
        let waiter = fixture.reload().await;
        let outcome = fixture.outcomes.recv().await.unwrap();

        let (commands_tx, mut commands) = mpsc::channel(8);
        let ((), (answered_after, reply)) = tokio::join!(
            fixture.host.finish_run(outcome, &mut commands),
            probe(&commands_tx, Duration::from_millis(300), ApiRequest::Status),
        );

        assert!(
            answered_after < Duration::from_millis(1500),
            "{answered_after:?}"
        );
        assert!(matches!(reply, ApiResponse::Status { .. }));
        assert!(message(&waiter.await.unwrap()).contains("1 added"));

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_first_load_that_outlasts_the_deadline_is_a_load_error_and_frees_the_queue() {
        let mut fixture = Fixture::with_fifo().with_deadline(Duration::from_millis(300));
        fixture.host.begin_first_load();
        let pending = fixture.reload().await;

        fixture.next_outcome().await;

        let state = fixture.host.state();
        assert_eq!(state.status, DaemonStatus::Starting);
        let error = state.load_error.expect("a timeout is a load error");
        assert_eq!(error.code, "env_file_read_timeout");
        assert!(error.message.contains("group 'app' env_file"), "{error:?}");
        assert!(error.message.contains("slow.env"), "{error:?}");
        assert!(error.message.contains("did not finish within"), "{error:?}");
        assert!(error.hint.unwrap().contains("named pipe"));

        // The pending reload started when the stuck read was abandoned. It reads the same pipe, so
        // it times out too and answers its waiter.
        assert!(fixture.host.in_flight.is_some());
        assert!(fixture.host.pending.is_none());

        fixture.next_outcome().await;

        let response = pending.await.unwrap();
        assert!(
            matches!(response, ApiResponse::Error { .. }),
            "{response:?}"
        );
        assert!(
            message(&response).contains("env_file_read_timeout"),
            "{response:?}"
        );
        assert!(fixture.host.in_flight.is_none());

        // Both timeouts are observed, so the swap cannot race a read opening the pipe. The
        // abandoned threads stay behind on the old pipe.
        let env_path = fixture.dir.path().join("slow.env");
        std::fs::remove_file(&env_path).unwrap();
        std::fs::write(&env_path, "A=1\n").unwrap();

        let recovered = fixture.reload().await;
        fixture.next_outcome().await;

        let response = recovered.await.unwrap();
        assert!(
            message(&response).starts_with("config reloaded"),
            "{response:?}"
        );
        assert!(fixture.host.is_loaded());

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_reload_that_outlasts_the_deadline_keeps_the_running_config() {
        let mut fixture = Fixture::with_fifo().with_deadline(Duration::from_millis(400));
        fixture.host.begin_first_load();
        fixture.feed("A=1\n");
        fixture.next_outcome().await;
        assert!(fixture.host.is_loaded());

        let waiter = fixture.reload().await;
        fixture.next_outcome().await;

        let response = waiter.await.unwrap();
        assert!(matches!(response, ApiResponse::Error { .. }));
        assert!(
            message(&response).contains("env_file_read_timeout"),
            "{response:?}"
        );

        let state = fixture.host.state();
        assert_eq!(state.status, DaemonStatus::Running);
        assert!(state.load_error.is_none());
        assert!(state.groups.contains_key("app"));
        assert!(fixture.host.in_flight.is_none());

        fixture.feed("A=1\n");
        fixture.host.shutdown().await.unwrap();
    }

    const LOGGING_CONFIG: &str = "[[group]]\nname = \"chatty\"\n\n\
                                  [[group.task]]\nname = \"say\"\ncommand = \"echo hello-from-task\"\n\n\
                                  [[group]]\nname = \"svc\"\n\n\
                                  [[group.service]]\nname = \"s\"\ncommand = \"sleep 30\"\ndelay = \"3s\"\n";

    #[tokio::test]
    async fn logs_answer_in_full_while_the_first_startup_applies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zaz.toml"), LOGGING_CONFIG).unwrap();
        let mut fixture = Fixture::at(dir);
        fixture.host.begin_first_load();
        let outcome = fixture.outcomes.recv().await.unwrap();

        let (commands_tx, mut commands) = mpsc::channel(8);
        let poll = async {
            for _ in 0..20 {
                let (_, reply) = probe(
                    &commands_tx,
                    Duration::from_millis(100),
                    ApiRequest::GetLogs {
                        name: "*".to_string(),
                        project: None,
                        lines: None,
                        offset: None,
                        limit: None,
                        search: None,
                    },
                )
                .await;

                if let ApiResponse::Logs { lines, .. } = reply {
                    if lines.iter().any(|l| l.content.contains("hello-from-task")) {
                        return true;
                    }
                }
            }

            false
        };
        let ((), seen) = tokio::join!(fixture.host.finish_run(outcome, &mut commands), poll);

        assert!(
            seen,
            "the task's output should be readable during the apply"
        );

        fixture.host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_panic_during_evaluation_is_a_load_error() {
        let mut fixture = Fixture::with_fifo();
        fixture.host.begin_first_load();
        let id = fixture.host.in_flight.as_ref().unwrap().id;

        let (tx, rx) = oneshot::channel();
        evaluate::run_detached(
            || panic!("evaluation exploded"),
            move |result| {
                let _ = tx.send(result);
            },
        );
        let result = rx.await.unwrap();
        assert!(matches!(result, Err(DaemonError::EvaluationPanicked)));

        fixture
            .host
            .finish_run(RunOutcome { id, result }, &mut fixture.commands)
            .await;

        let state = fixture.host.state();
        assert_eq!(state.status, DaemonStatus::Starting);
        assert_eq!(state.load_error.unwrap().code, "evaluation_panicked");
        assert!(fixture.host.in_flight.is_none());
    }
}
