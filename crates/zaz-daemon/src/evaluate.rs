//! The evaluation step.
//!
//! `zaz_config::load` only parses and validates. Reading the sources a config names happens here,
//! once per load or reload, and produces the environment every group is built from. Group build
//! takes an [`EvaluatedEnv`] as a required input, so no group can be built without one.
//!
//! [`evaluate`] is a plain function of the config and the config path. It blocks on file reads and
//! on commands, and a caller that must not block runs it on its own thread.
//!
//! Commands run one at a time, by level and then in config order: settings, then groups, then
//! tasks and services. Each runs in exactly the environment its output is applied to, so the
//! command of a lower level sees everything applied above it.

use crate::env::{self, EnvLayer, EnvMap, GroupEnv, Passthrough, ProcessRef, Start};
use crate::env_command::{self, EnvCommandError, EnvCommandErrorKind};
use crate::env_report::{
    EnvLevel, EnvReport, EnvSourceKind, EnvSourceReport, EnvStart, GroupEnvReport, ProcessEnvReport,
};
use crate::sources;
use crate::state::{DaemonStatus, LoadError};
use crate::DaemonError;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zaz_config::{Config, EnvFileLevel, SpecialFiles};
use zaz_process::CancelHandle;

/// A config loaded from disk together with everything its sources evaluated to.
///
/// This is all the slow work of a load or reload. Building groups from it is quick and runs on
/// the daemon's event loop.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub(crate) config: Config,
    pub(crate) evaluated: EvaluatedEnv,
}

/// How long one `env_file` read may run before it is abandoned.
///
/// A file that never finishes, such as a named pipe with no writer, would otherwise hold the load
/// for good, and every later reload would queue behind it. The limit applies to each read on its
/// own, so the worst case for a config is one limit per distinct file.
pub(crate) const ENV_FILE_READ_DEADLINE: Duration = Duration::from_secs(120);

/// How much of the sources an evaluation reads again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Read the config and run every command.
    Full,

    /// Read the sources only, and run the commands the changed paths make stale.
    ///
    /// The paths are the sources the watcher reported, in its normal form.
    Partial(BTreeSet<PathBuf>),
}

impl Scope {
    /// The scope of one run that serves two requests.
    ///
    /// A full run covers everything a partial one would, so it absorbs it.
    pub(crate) fn merge(self, other: Scope) -> Scope {
        match (self, other) {
            (Scope::Partial(mut a), Scope::Partial(b)) => {
                a.extend(b);
                Scope::Partial(a)
            }
            _ => Scope::Full,
        }
    }
}

/// What each evaluation node produced last time, kept so a later run can reuse it.
///
/// A node is one level's `env_command`. A full load starts from an empty cache, so it runs every
/// command. A partial run edits the cache in place.
#[derive(Debug, Default)]
pub(crate) struct EvalCache {
    nodes: HashMap<EnvFileLevel, Node>,
}

#[derive(Debug)]
struct Node {
    key: CacheKey,
    hashes: Vec<(PathBuf, FileHash)>,
    layer: Option<EnvLayer>,
}

/// The content of one watched file, taken before a command spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileHash {
    Absent,
    Digest([u8; 32]),
    /// A read error, a directory, or a file that is not a regular file. It never matches.
    Unreadable,
}

fn hash_file(path: &Path) -> FileHash {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileHash::Absent,
        Err(_) => return FileHash::Unreadable,
    };

    if !meta.is_file() {
        return FileHash::Unreadable;
    }

    let Ok(mut file) = std::fs::File::open(path) else {
        return FileHash::Unreadable;
    };

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];

    loop {
        match file.read(&mut buffer) {
            Ok(0) => return FileHash::Digest(hasher.finalize().into()),
            Ok(n) => hasher.update(&buffer[..n]),
            Err(_) => return FileHash::Unreadable,
        }
    }
}

fn hash_files(paths: &[PathBuf]) -> Vec<(PathBuf, FileHash)> {
    paths
        .iter()
        .map(|path| (path.clone(), hash_file(path)))
        .collect()
}

fn same_content(old: &[(PathBuf, FileHash)], new: &[(PathBuf, FileHash)]) -> bool {
    old.len() == new.len()
        && old
            .iter()
            .zip(new)
            .all(|(a, b)| a.0 == b.0 && a.1 == b.1 && a.1 != FileHash::Unreadable)
}

/// Load the config at `config_path` and evaluate the sources it names.
///
/// `config_path` must be absolute. This blocks on file reads, each for at most `read_deadline`, and
/// on commands, each under its own timeout. `cancel` stops the command that is running.
pub(crate) fn prepare(
    config_path: &Path,
    read_deadline: Duration,
    cancel: &CancelHandle,
) -> Result<Prepared, DaemonError> {
    prepare_scoped(
        config_path,
        read_deadline,
        cancel,
        &Scope::Full,
        None,
        &Mutex::new(EvalCache::default()),
    )
}

/// Evaluate `scope`, reusing the nodes of `cache` that a partial scope leaves clean.
///
/// A full scope reads `config_path` and runs every command. A partial scope never reads it, and
/// evaluates `config`, the config the daemon is running.
///
/// The cache is shared with the host, so the nodes that finished survive a failure, and even a
/// panic, of this evaluation.
pub(crate) fn prepare_scoped(
    config_path: &Path,
    read_deadline: Duration,
    cancel: &CancelHandle,
    scope: &Scope,
    config: Option<Config>,
    cache: &Mutex<EvalCache>,
) -> Result<Prepared, DaemonError> {
    let config = match config {
        Some(config) => config,
        None => zaz_config::load(config_path).map_err(DaemonError::Config)?,
    };

    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    let evaluated = evaluate_scoped(
        &config,
        config_path,
        read_deadline,
        cancel,
        scope,
        &mut cache,
    )?;

    Ok(Prepared { config, evaluated })
}

/// Run [`prepare`] on its own thread and deliver the result to `done`.
///
/// See [`run_detached`] for the thread and the panic handling.
pub(crate) fn prepare_detached<F>(
    config_path: PathBuf,
    read_deadline: Duration,
    cancel: CancelHandle,
    done: F,
) where
    F: FnOnce(Result<Prepared, DaemonError>) + Send + 'static,
{
    run_detached(move || prepare(&config_path, read_deadline, &cancel), done);
}

/// Run [`prepare_scoped`] on its own thread and deliver the result to `done`.
///
/// `scope` and `config` select a full or a partial run.
pub(crate) fn prepare_scoped_detached<F>(
    config_path: PathBuf,
    read_deadline: Duration,
    cancel: CancelHandle,
    scope: Scope,
    config: Option<Config>,
    cache: Arc<Mutex<EvalCache>>,
    done: F,
) where
    F: FnOnce(Result<Prepared, DaemonError>) + Send + 'static,
{
    run_detached(
        move || prepare_scoped(&config_path, read_deadline, &cancel, &scope, config, &cache),
        done,
    );
}

/// Run `work` on its own detached thread and deliver its result to `done`.
///
/// The thread is detached, not a runtime blocking task, so the runtime can shut down while it
/// runs. `work` bounds its own blocking reads and commands, so the thread always finishes.
///
/// `done` runs exactly once. A panic in `work`, or a thread that cannot start, reaches it as an
/// error.
pub(crate) fn run_detached<T, W, F>(work: W, done: F)
where
    T: Send + 'static,
    W: FnOnce() -> Result<T, DaemonError> + Send + 'static,
    F: FnOnce(Result<T, DaemonError>) + Send + 'static,
{
    // A failed spawn drops the closure, and `done` with it. The slot keeps `done` reachable so the
    // failure can still be delivered.
    let slot = Arc::new(Mutex::new(Some(done)));
    let worker_slot = Arc::clone(&slot);

    let spawned = std::thread::Builder::new()
        .name("zaz-evaluate".to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or(Err(DaemonError::EvaluationPanicked));

            if let Some(done) = take_done(&worker_slot) {
                done(result);
            }
        });

    if let Err(e) = spawned {
        tracing::error!(error = %e, "failed to start the evaluation thread");

        if let Some(done) = take_done(&slot) {
            done(Err(DaemonError::EvaluationThread(e)));
        }
    }
}

fn take_done<F>(slot: &Mutex<Option<F>>) -> Option<F> {
    slot.lock().ok().and_then(|mut slot| slot.take())
}

/// The environment layers every group of a config is built from.
///
/// It also records where the layers came from. The records feed the loaded-keys view, and the load
/// logs them. They never hold values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EvaluatedEnv {
    groups: HashMap<String, GroupEnv>,
    sources: Vec<SourceRecord>,
    dropped: Vec<DroppedRecord>,
}

impl EvaluatedEnv {
    /// The evaluated layers of a group, by name.
    pub(crate) fn group(&self, name: &str) -> Option<&GroupEnv> {
        self.groups.get(name)
    }
}

impl EvaluatedEnv {
    /// The loaded-keys report of `config`, which must be the config this was evaluated from.
    ///
    /// A process lists the sources of its whole chain, settings first. Within a level the order is
    /// file, command, then inline `env`, which is the order they are applied in.
    pub(crate) fn report(
        &self,
        config: &Config,
        status: DaemonStatus,
        refresh_error: Option<LoadError>,
    ) -> EnvReport {
        let level_sources = |level: EnvFileLevel, inline: &HashMap<String, String>| {
            let mut sources: Vec<EnvSourceReport> = self
                .sources
                .iter()
                .filter(|record| record.level == level)
                .map(|record| record.to_report())
                .collect();

            if !inline.is_empty() {
                sources.push(inline_report(level, inline, config));
            }

            sources
        };
        let no_inline = HashMap::new();
        let settings = level_sources(EnvFileLevel::Settings, &no_inline);

        let groups = config
            .groups
            .iter()
            .enumerate()
            .map(|(g, group)| {
                let env = self.group(&group.name);
                let start_of = |start: Option<&Start>| match start {
                    Some(start) if start.is_base() => EnvStart::Base,
                    _ => EnvStart::Daemon,
                };

                let mut group_sources = settings.clone();
                group_sources.extend(level_sources(EnvFileLevel::Group { group: g }, &group.env));

                let tasks = group
                    .tasks
                    .iter()
                    .enumerate()
                    .map(|(t, task)| {
                        let mut sources = group_sources.clone();
                        sources.extend(level_sources(
                            EnvFileLevel::Task { group: g, task: t },
                            &task.env,
                        ));

                        ProcessEnvReport {
                            name: task.name().to_string(),
                            starts_from: start_of(
                                env.map(|env| env.process_start(ProcessRef::Task(t)))
                                    .as_ref(),
                            ),
                            sources,
                        }
                    })
                    .collect();

                let services = group
                    .services
                    .iter()
                    .enumerate()
                    .map(|(s, service)| {
                        let mut sources = group_sources.clone();
                        sources.extend(level_sources(
                            EnvFileLevel::Service {
                                group: g,
                                service: s,
                            },
                            &service.env,
                        ));

                        ProcessEnvReport {
                            name: service.name().to_string(),
                            starts_from: start_of(
                                env.map(|env| env.process_start(ProcessRef::Service(s)))
                                    .as_ref(),
                            ),
                            sources,
                        }
                    })
                    .collect();

                GroupEnvReport {
                    name: group.name.clone(),
                    starts_from: start_of(env.map(|env| env.group_start())),
                    sources: group_sources,
                    tasks,
                    services,
                }
            })
            .collect();

        EnvReport {
            project: None,
            status,
            load_error: None,
            refresh_error,
            dropped_inherited: self
                .dropped
                .first()
                .map(|record| record.names.clone())
                .unwrap_or_default(),
            groups,
        }
    }
}

fn inline_report(
    level: EnvFileLevel,
    inline: &HashMap<String, String>,
    config: &Config,
) -> EnvSourceReport {
    let mut set: Vec<String> = inline.keys().cloned().collect();
    set.sort();

    let (kind_level, field) = match level {
        EnvFileLevel::Settings => (EnvLevel::Settings, "settings env".to_string()),
        EnvFileLevel::Group { group } => (
            EnvLevel::Group,
            format!("group '{}' env", config.groups[group].name),
        ),
        EnvFileLevel::Task { group, task } => {
            let group = &config.groups[group];
            (
                EnvLevel::Task,
                format!(
                    "group '{}' task '{}' env",
                    group.name,
                    group.tasks[task].name()
                ),
            )
        }
        EnvFileLevel::Service { group, service } => {
            let group = &config.groups[group];
            (
                EnvLevel::Service,
                format!(
                    "group '{}' service '{}' env",
                    group.name,
                    group.services[service].name()
                ),
            )
        }
    };

    EnvSourceReport {
        level: kind_level,
        field,
        kind: EnvSourceKind::Inline,
        origin: None,
        set,
        unset: Vec::new(),
        produced_no_keys: false,
    }
}

/// Which kind of source a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    File,
    Command,
}

impl SourceKind {
    fn label(self) -> &'static str {
        match self {
            Self::File => "env_file",
            Self::Command => "env_command",
        }
    }
}

/// The names one source set and unset. A source with neither produced no variables.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceRecord {
    level: EnvFileLevel,
    field: String,
    kind: SourceKind,
    origin: String,
    set: Vec<String>,
    unset: Vec<String>,
}

impl SourceRecord {
    fn to_report(&self) -> EnvSourceReport {
        EnvSourceReport {
            level: match self.level {
                EnvFileLevel::Settings => EnvLevel::Settings,
                EnvFileLevel::Group { .. } => EnvLevel::Group,
                EnvFileLevel::Task { .. } => EnvLevel::Task,
                EnvFileLevel::Service { .. } => EnvLevel::Service,
            },
            field: self.field.clone(),
            kind: match self.kind {
                SourceKind::File => EnvSourceKind::File,
                SourceKind::Command => EnvSourceKind::Command,
            },
            origin: Some(self.origin.clone()),
            set: self.set.clone(),
            unset: self.unset.clone(),
            produced_no_keys: self.set.is_empty() && self.unset.is_empty(),
        }
    }
}

/// The inherited names the base environment dropped for the level that selected it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DroppedRecord {
    field: String,
    names: Vec<String>,
}

/// Read every source `config` names and build the layers of each group.
///
/// `config_path` must be absolute. A relative `env_file` resolves against its directory. Each
/// distinct file is read once, so every level that names it sees the same bytes. A failing file
/// fails the evaluation. Every file is still read, so all errors are reported. Each read gets its
/// own `read_deadline`.
///
/// Commands run after the files, one at a time. The first failing command fails the evaluation,
/// since the commands below it would run in an environment that never existed.
pub(crate) fn evaluate(
    config: &Config,
    config_path: &Path,
    read_deadline: Duration,
    cancel: &CancelHandle,
) -> Result<EvaluatedEnv, DaemonError> {
    evaluate_scoped(
        config,
        config_path,
        read_deadline,
        cancel,
        &Scope::Full,
        &mut EvalCache::default(),
    )
}

/// [`evaluate`] for a given scope, reading and updating `cache`.
///
/// Files are read again every time, since a read is cheap and its parse decides whether anything
/// below it changed. A command runs again when nothing cached covers it. A partial scope reuses
/// every node the changed paths leave clean.
pub(crate) fn evaluate_scoped(
    config: &Config,
    config_path: &Path,
    read_deadline: Duration,
    cancel: &CancelHandle,
    scope: &Scope,
    cache: &mut EvalCache,
) -> Result<EvaluatedEnv, DaemonError> {
    let reads = zaz_config::read_env_files(
        config,
        config_path,
        SpecialFiles::Read,
        Some(read_deadline),
        |field, path| {
            tracing::info!(
                config = %config_path.display(),
                field = %field,
                path = %path.display(),
                "loading env_file"
            );
        },
    );

    if !reads.errors.is_empty() {
        return Err(DaemonError::EnvFile(reads.errors));
    }

    let mut sources = Vec::new();
    let mut files: HashMap<EnvFileLevel, EnvLayer> = HashMap::new();

    for source in reads.sources() {
        if let Some(vars) = reads.vars(source) {
            let layer = EnvLayer::from_vars(vars);

            sources.push(SourceRecord {
                level: source.level,
                field: source.field.clone(),
                kind: SourceKind::File,
                origin: source.configured.clone(),
                set: layer.set_keys(),
                unset: Vec::new(),
            });
            files.insert(source.level, layer);
        }
    }

    let mut run = CommandRun::new(config, config_path, cancel, scope, cache);
    let mut commands: HashMap<EnvFileLevel, EnvLayer> = HashMap::new();
    let mut dropped = Vec::new();

    for source in run.ordered_sources() {
        let input = run.input_env(&source.level, &files, &commands);
        let layer = run.execute(&source, input)?;

        sources.push(SourceRecord {
            level: source.level,
            field: source.field.clone(),
            kind: SourceKind::Command,
            origin: source.command.clone(),
            set: layer.set_keys(),
            unset: layer.unset_keys(),
        });
        dropped.push(DroppedRecord {
            field: source.field.clone(),
            names: env::dropped_names(&run.passthrough),
        });
        commands.insert(source.level, layer);
    }

    let groups = build_groups(config, &run.passthrough, &files, &commands);
    let evaluated = EvaluatedEnv {
        groups,
        sources,
        dropped,
    };

    if *scope == Scope::Full {
        log_provenance(&evaluated);
    }

    Ok(evaluated)
}

fn log_provenance(evaluated: &EvaluatedEnv) {
    for record in &evaluated.dropped {
        tracing::info!(
            field = %record.field,
            count = record.names.len(),
            names = %record.names.join(","),
            "env_command starts from the base environment, so these inherited variables do not \
             reach its processes"
        );
    }

    for record in &evaluated.sources {
        if record.set.is_empty() && record.unset.is_empty() {
            tracing::info!(
                field = %record.field,
                kind = record.kind.label(),
                "source produced no variables"
            );
        } else {
            tracing::debug!(
                field = %record.field,
                kind = record.kind.label(),
                set = %record.set.join(","),
                unset = %record.unset.join(","),
                "source loaded"
            );
        }
    }
}

/// The layers of every group, with the environment each chain starts from.
///
/// A chain starts from the base when any level of it sets an `env_command`, and from the
/// daemon's environment otherwise. Within a level the order is file, command output, inline `env`.
fn build_groups(
    config: &Config,
    passthrough: &Passthrough,
    files: &HashMap<EnvFileLevel, EnvLayer>,
    commands: &HashMap<EnvFileLevel, EnvLayer>,
) -> HashMap<String, GroupEnv> {
    let level_layers = |level: EnvFileLevel| -> Vec<EnvLayer> {
        files
            .get(&level)
            .into_iter()
            .chain(commands.get(&level))
            .cloned()
            .collect()
    };
    let start_of = |from_base: bool| {
        if from_base {
            Start::Base(passthrough.clone())
        } else {
            Start::Inherited
        }
    };

    let settings_from_base = config.settings.env_command.is_some();
    let mut groups = HashMap::new();

    for (g, group) in config.groups.iter().enumerate() {
        let group_from_base = settings_from_base || group.env_command.is_some();

        let mut chain = level_layers(EnvFileLevel::Settings);
        chain.extend(level_layers(EnvFileLevel::Group { group: g }));
        chain.push(EnvLayer::from_vars(&group.env));

        let tasks = group
            .tasks
            .iter()
            .enumerate()
            .map(|(t, task)| {
                let mut layers = level_layers(EnvFileLevel::Task { group: g, task: t });
                layers.push(EnvLayer::from_vars(&task.env));

                (
                    start_of(group_from_base || task.env_command.is_some()),
                    layers,
                )
            })
            .collect();

        let services = group
            .services
            .iter()
            .enumerate()
            .map(|(s, service)| {
                let mut layers = level_layers(EnvFileLevel::Service {
                    group: g,
                    service: s,
                });
                layers.push(EnvLayer::from_vars(&service.env));

                (
                    start_of(group_from_base || service.env_command.is_some()),
                    layers,
                )
            })
            .collect();

        groups.insert(
            group.name.clone(),
            GroupEnv::with_starts(start_of(group_from_base), chain, tasks, services),
        );
    }

    groups
}

/// A command's identity for sharing within one load.
///
/// The input environment is part of the key, in sorted form, because a lower level's command runs
/// in what was applied above it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    command: String,
    working_dir: PathBuf,
    env: Vec<(OsString, OsString)>,
}

/// The state shared by the commands of one load.
struct CommandRun<'a> {
    scope: &'a Scope,
    cache: &'a mut EvalCache,
    watch: HashMap<EnvFileLevel, Vec<PathBuf>>,
    dirty: HashSet<EnvFileLevel>,
    config: &'a Config,
    config_path: &'a Path,
    config_dir: PathBuf,
    cancel: &'a CancelHandle,
    shell: String,
    timeout: Duration,
    passthrough: Passthrough,
    context: zaz_vars::Context,
    shared: HashMap<CacheKey, EnvLayer>,
}

impl<'a> CommandRun<'a> {
    fn new(
        config: &'a Config,
        config_path: &'a Path,
        cancel: &'a CancelHandle,
        scope: &'a Scope,
        cache: &'a mut EvalCache,
    ) -> Self {
        let config_dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        Self {
            scope,
            cache,
            watch: sources::command_watch_paths(config, &config_dir),
            dirty: HashSet::new(),
            config,
            config_path,
            cancel,
            shell: config
                .settings
                .shell
                .clone()
                .unwrap_or_else(|| "/bin/sh".to_string()),
            timeout: config
                .settings
                .env_command_timeout
                .map_or(env_command::DEFAULT_TIMEOUT, |t| t.as_duration()),
            passthrough: Passthrough::new(&config.settings.env_passthrough),
            context: zaz_vars::Context::new()
                .with_variables(config.variables.clone())
                .with_root(config_dir.clone()),
            config_dir,
            shared: HashMap::new(),
        }
    }

    /// The commands in the order they run: settings, then groups, then tasks and services. Within
    /// a level the order is the config's.
    fn ordered_sources(&self) -> Vec<zaz_config::EnvCommandSource> {
        let mut sources = zaz_config::env_command_sources(self.config, &self.config_dir);
        sources.sort_by_key(|source| match source.level {
            EnvFileLevel::Settings => 0,
            EnvFileLevel::Group { .. } => 1,
            EnvFileLevel::Task { .. } | EnvFileLevel::Service { .. } => 2,
        });

        sources
    }

    /// The environment the command at `level` runs in, which is the one its output lands on.
    ///
    /// That is the base plus every layer applied above the command: the settings file and
    /// command, then the group's file, command, and inline `env`, then the process's own file.
    fn input_env(
        &self,
        level: &EnvFileLevel,
        files: &HashMap<EnvFileLevel, EnvLayer>,
        commands: &HashMap<EnvFileLevel, EnvLayer>,
    ) -> EnvMap {
        let settings = EnvFileLevel::Settings;
        let mut layers: Vec<EnvLayer> = Vec::new();

        layers.extend(files.get(&settings).cloned());

        match *level {
            EnvFileLevel::Settings => {}
            EnvFileLevel::Group { group } => {
                layers.extend(commands.get(&settings).cloned());
                layers.extend(files.get(&EnvFileLevel::Group { group }).cloned());
            }
            EnvFileLevel::Task { group, .. } | EnvFileLevel::Service { group, .. } => {
                layers.extend(commands.get(&settings).cloned());
                layers.extend(files.get(&EnvFileLevel::Group { group }).cloned());
                layers.extend(commands.get(&EnvFileLevel::Group { group }).cloned());
                layers.extend(
                    self.config
                        .groups
                        .get(group)
                        .map(|group| EnvLayer::from_vars(&group.env)),
                );
                layers.extend(files.get(level).cloned());
            }
        }

        env::build_env(env::base_env(&self.passthrough), &layers)
    }

    /// Whether a changed source names this level's command.
    fn named_by_change(&self, level: EnvFileLevel) -> bool {
        let Scope::Partial(changed) = self.scope else {
            return false;
        };

        self.watch.get(&level).is_some_and(|paths| {
            paths
                .iter()
                .any(|path| changed.contains(&zaz_watch::normalize(path)))
        })
    }

    /// Whether a command above this level ran again in this evaluation.
    fn above_is_dirty(&self, level: EnvFileLevel) -> bool {
        let settings = self.dirty.contains(&EnvFileLevel::Settings);

        match level {
            EnvFileLevel::Settings => false,
            EnvFileLevel::Group { .. } => settings,
            EnvFileLevel::Task { group, .. } | EnvFileLevel::Service { group, .. } => {
                settings || self.dirty.contains(&EnvFileLevel::Group { group })
            }
        }
    }

    /// The cached layer of this level when nothing makes it stale.
    ///
    /// A node is stale when it has no result, when its command, directory, or input environment
    /// differ, when a command above it ran again, or when a changed source names it and the
    /// content of its watched files differs. A node whose last run failed is never reused.
    fn reusable(&self, level: EnvFileLevel, key: &CacheKey) -> Option<EnvLayer> {
        if *self.scope == Scope::Full {
            return None;
        }

        let node = self.cache.nodes.get(&level)?;
        let layer = node.layer.as_ref()?;

        if node.key != *key || self.above_is_dirty(level) {
            return None;
        }

        if self.named_by_change(level) {
            let paths = self.watch.get(&level).map(Vec::as_slice).unwrap_or(&[]);

            if !same_content(&node.hashes, &hash_files(paths)) {
                return None;
            }

            tracing::info!(
                level = ?level,
                "watched files have the same content, so the env_command does not run again"
            );
        }

        Some(layer.clone())
    }

    /// Run one command, or reuse the output of an earlier run.
    fn execute(
        &mut self,
        source: &zaz_config::EnvCommandSource,
        input: EnvMap,
    ) -> Result<EnvLayer, DaemonError> {
        let fail = |kind: EnvCommandErrorKind| {
            DaemonError::EnvCommand(Box::new(EnvCommandError {
                config: self.config_path.to_path_buf(),
                field: source.field.clone(),
                working_dir: source.working_dir.clone(),
                kind,
            }))
        };

        let command = zaz_vars::Expander::new(&self.context)
            .expand(&source.command)
            .map_err(|e| {
                fail(EnvCommandErrorKind::Expansion {
                    detail: e.to_string(),
                })
            })?;

        let mut sorted: Vec<(OsString, OsString)> = input.clone().into_iter().collect();
        sorted.sort();

        let key = CacheKey {
            command,
            working_dir: source.working_dir.clone(),
            env: sorted,
        };

        if let Some(layer) = self.reusable(source.level, &key) {
            return Ok(layer);
        }

        self.dirty.insert(source.level);

        let paths = self.watch.get(&source.level).cloned().unwrap_or_default();
        let hashes = hash_files(&paths);

        if let Some(layer) = self.shared.get(&key) {
            tracing::debug!(field = %source.field, "reusing the output of an identical env_command");
            let layer = layer.clone();
            self.remember(source.level, key, hashes, Some(layer.clone()));

            return Ok(layer);
        }

        tracing::info!(
            config = %self.config_path.display(),
            field = %source.field,
            working_dir = %source.working_dir.display(),
            "running env_command"
        );

        let output = env_command::run(
            &env_command::RunSpec {
                shell: &self.shell,
                command: &key.command,
                working_dir: &source.working_dir,
                env: &input,
                timeout: self.timeout,
            },
            self.cancel,
        );

        match output {
            Ok(output) => {
                let layer = EnvLayer::from_output(output);
                self.shared.insert(key.clone(), layer.clone());
                self.remember(source.level, key, hashes, Some(layer.clone()));

                Ok(layer)
            }
            Err(kind) => {
                self.remember(source.level, key, Vec::new(), None);

                Err(fail(kind))
            }
        }
    }

    fn remember(
        &mut self,
        level: EnvFileLevel,
        key: CacheKey,
        hashes: Vec<(PathBuf, FileHash)>,
        layer: Option<EnvLayer>,
    ) {
        self.cache.nodes.insert(level, Node { key, hashes, layer });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(config: &Config, path: &Path, deadline: Duration) -> Result<EvaluatedEnv, DaemonError> {
        evaluate(config, path, deadline, &CancelHandle::new())
    }

    fn file_errors(error: DaemonError) -> Vec<zaz_config::EnvFileError> {
        match error {
            DaemonError::EnvFile(errors) => errors,
            other => panic!("expected env_file errors, got {other}"),
        }
    }

    fn config_in(dir: &Path, toml: &str) -> (Config, PathBuf) {
        let path = dir.join("zaz.toml");
        std::fs::write(&path, toml).unwrap();

        (zaz_config::load(&path).unwrap(), path)
    }

    #[test]
    fn a_config_with_no_env_file_evaluates_without_reading_anything() {
        let dir = tempfile::tempdir().unwrap();
        let (config, path) = config_in(
            dir.path(),
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv = { A = \"1\" }\n",
        );

        let evaluated = run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap();

        assert!(evaluated.group("a").is_some());
        assert!(evaluated.group("missing").is_none());
    }

    #[test]
    fn an_edited_file_changes_the_evaluated_group() {
        let dir = tempfile::tempdir().unwrap();
        let (config, path) = config_in(
            dir.path(),
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_file = \".env\"\n",
        );
        std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
        let before = run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap();

        std::fs::write(dir.path().join(".env"), "A=2\n").unwrap();
        let after = run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap();

        assert_ne!(before, after);

        std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
        assert_eq!(before, run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap());
    }

    #[test]
    fn a_missing_file_fails_the_evaluation_with_its_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let (config, path) = config_in(
            dir.path(),
            "[settings]\nenv_file = \"nope.env\"\n\n[[group]]\nname = \"a\"\npatterns = [\"*\"]\n",
        );

        let err = file_errors(run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap_err()).remove(0);

        assert_eq!(err.code(), "env_file_missing");
        assert_eq!(err.path, dir.path().join("nope.env"));
        assert_eq!(err.config, path);
        assert_eq!(err.field, "settings env_file");
    }

    #[test]
    fn every_read_gets_its_own_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (config, path) = config_in(
            dir.path(),
            "[settings]\nenv_file = \"one.env\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_file = \"two.env\"\n",
        );

        for name in ["one.env", "two.env"] {
            let status = std::process::Command::new("mkfifo")
                .arg(dir.path().join(name))
                .status()
                .unwrap();
            assert!(status.success(), "mkfifo is required for this test");
        }

        let errors = file_errors(run(&config, &path, Duration::from_millis(200)).unwrap_err());

        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|e| e.code() == "env_file_read_timeout"));
        assert_eq!(errors[0].path, dir.path().join("one.env"));
        assert_eq!(errors[1].path, dir.path().join("two.env"));
    }

    #[test]
    fn a_settings_file_reaches_every_group() {
        let dir = tempfile::tempdir().unwrap();
        let (config, path) = config_in(
            dir.path(),
            "[settings]\nenv_file = \".env\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\n\n\
             [[group]]\nname = \"b\"\npatterns = [\"*\"]\n",
        );
        std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
        let before = run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap();

        std::fs::write(dir.path().join(".env"), "A=2\n").unwrap();
        let after = run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap();

        assert_ne!(before.group("a"), after.group("a"));
        assert_ne!(before.group("b"), after.group("b"));
    }

    fn load_in(dir: &Path, toml: &str) -> (Config, PathBuf, EvaluatedEnv) {
        let (config, path) = config_in(dir, toml);
        let evaluated = run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap();

        (config, path, evaluated)
    }

    fn command_error(dir: &Path, toml: &str) -> EnvCommandError {
        let (config, path) = config_in(dir, toml);

        match run(&config, &path, ENV_FILE_READ_DEADLINE).unwrap_err() {
            DaemonError::EnvCommand(error) => *error,
            other => panic!("expected an env_command error, got {other}"),
        }
    }

    fn task_env(config: &Config, evaluated: &EvaluatedEnv, task: usize) -> EnvMap {
        let dir = Path::new("/cfg");
        let group = &config.groups[0];
        let env = evaluated.group(&group.name).unwrap();
        let executor = env::group_executor(None, dir, None, env);

        env::process_executor(&executor, dir, None, env, env::ProcessRef::Task(task))
            .env()
            .clone()
    }

    fn get<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
        env.get(std::ffi::OsStr::new(key)).and_then(|v| v.to_str())
    }

    #[test]
    fn a_command_sets_and_unsets_and_empty_output_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (config, _, evaluated) = load_in(
            dir.path(),
            "[settings]\nenv_command = \"printf '{\\\"NEW\\\":\\\"1\\\",\\\"HOME\\\":null}'\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = \"true\"\n\n\
             [[group.task]]\ncommand = \"x\"\n",
        );

        let env = task_env(&config, &evaluated, 0);

        assert_eq!(get(&env, "NEW"), Some("1"));
        assert_eq!(get(&env, "HOME"), None);
        assert!(get(&env, "PATH").is_some());
    }

    #[test]
    fn a_chain_with_a_command_starts_from_the_base_and_other_chains_stay_inherited() {
        std::env::set_var("ZAZ_UT_OUTSIDE_BASE", "yes");
        std::env::set_var("ZAZ_UT_PASS_ONE", "1");
        std::env::set_var("ZAZ_UT_PASS_PREFIX_X", "2");

        let dir = tempfile::tempdir().unwrap();
        let (config, path, evaluated) = load_in(
            dir.path(),
            "[settings]\nenv_passthrough = [\"ZAZ_UT_PASS_ONE\", \"ZAZ_UT_PASS_PREFIX_*\"]\n\n\
             [[group]]\nname = \"plain\"\npatterns = [\"*\"]\n\n\
             [[group.task]]\ncommand = \"x\"\n\n\
             [[group]]\nname = \"cmd\"\npatterns = [\"*\"]\n\n\
             [[group.task]]\ncommand = \"x\"\nenv_command = \"true\"\n\n\
             [[group.task]]\ncommand = \"y\"\n",
        );
        let _ = path;

        let plain = evaluated.group("plain").unwrap();
        let cmd = evaluated.group("cmd").unwrap();
        let dir = Path::new("/cfg");

        let plain_group = env::group_executor(None, dir, None, plain);
        let plain_task =
            env::process_executor(&plain_group, dir, None, plain, env::ProcessRef::Task(0));
        assert_eq!(get(plain_task.env(), "ZAZ_UT_OUTSIDE_BASE"), Some("yes"));

        let cmd_group = env::group_executor(None, dir, None, cmd);
        assert_eq!(get(cmd_group.env(), "ZAZ_UT_OUTSIDE_BASE"), Some("yes"));

        let with_command =
            env::process_executor(&cmd_group, dir, None, cmd, env::ProcessRef::Task(0));
        assert_eq!(get(with_command.env(), "ZAZ_UT_OUTSIDE_BASE"), None);
        assert_eq!(get(with_command.env(), "ZAZ_UT_PASS_ONE"), Some("1"));
        assert_eq!(get(with_command.env(), "ZAZ_UT_PASS_PREFIX_X"), Some("2"));

        let without = env::process_executor(&cmd_group, dir, None, cmd, env::ProcessRef::Task(1));
        assert_eq!(get(without.env(), "ZAZ_UT_OUTSIDE_BASE"), Some("yes"));

        assert_eq!(config.groups.len(), 2);
    }

    #[test]
    fn a_lower_command_runs_in_what_the_upper_levels_applied() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("diff-stub");
        std::fs::write(
            &stub,
            "#!/bin/sh\nif [ \"${A-}\" = 1 ]; then printf '{\"A\":null,\"B\":null,\"C\":\"3\"}'; \
             else printf '{\"C\":\"3\"}'; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        let toml = format!(
            "[settings]\nenv_command = \"printf '{{\\\"A\\\":\\\"1\\\",\\\"B\\\":\\\"2\\\"}}'\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = \"{}\"\n\n\
             [[group.task]]\ncommand = \"x\"\n",
            stub.display()
        );
        let (config, _, evaluated) = load_in(dir.path(), &toml);

        let env = task_env(&config, &evaluated, 0);

        assert_eq!(get(&env, "A"), None);
        assert_eq!(get(&env, "B"), None);
        assert_eq!(get(&env, "C"), Some("3"));
    }

    #[test]
    fn an_inline_env_above_a_command_is_its_input_and_the_command_output_beats_it() {
        let dir = tempfile::tempdir().unwrap();
        let (config, _, evaluated) = load_in(
            dir.path(),
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv = { X = \"group-inline\" }\n\n\
             [[group.task]]\ncommand = \"x\"\n\
             env_command = \"printf '{\\\"SEEN\\\":\\\"%s\\\",\\\"X\\\":\\\"cmd\\\"}' \\\"$X\\\"\"\n",
        );

        let env = task_env(&config, &evaluated, 0);

        assert_eq!(get(&env, "SEEN"), Some("group-inline"));
        assert_eq!(get(&env, "X"), Some("cmd"));
    }

    #[test]
    fn commands_run_one_at_a_time_by_level_then_in_config_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("order.log");
        let cmd = |name: &str| format!("echo {name} >> {}; printf '{{}}'", log.display());

        let toml = format!(
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = \"{ga}\"\n\n\
             [[group.task]]\ncommand = \"x\"\nenv_command = \"{ta}\"\n\n\
             [[group.service]]\ncommand = \"x\"\nenv_command = \"{sa}\"\n\n\
             [[group]]\nname = \"b\"\npatterns = [\"*\"]\nenv_command = \"{gb}\"\n\n\
             [[group.task]]\ncommand = \"x\"\nenv_command = \"{tb}\"\n\n\
             [settings]\nenv_command = \"{s}\"\n",
            ga = cmd("group-a"),
            ta = cmd("task-a"),
            sa = cmd("service-a"),
            gb = cmd("group-b"),
            tb = cmd("task-b"),
            s = cmd("settings"),
        );
        load_in(dir.path(), &toml);

        let order = std::fs::read_to_string(&log).unwrap();
        let order: Vec<&str> = order.lines().collect();

        assert_eq!(
            order,
            [
                "settings",
                "group-a",
                "group-b",
                "task-a",
                "service-a",
                "task-b"
            ]
        );
    }

    #[test]
    fn each_level_runs_in_its_own_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::create_dir_all(dir.path().join("task")).unwrap();
        let pwd = r#"printf "{\"CWD\":\"%s\"}" "$(pwd -P)""#;

        let toml = format!(
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nworking_dir = \"sub\"\nenv_command = '{pwd}'\n\n\
             [[group.task]]\ncommand = \"x\"\n\n\
             [[group.task]]\ncommand = \"y\"\nworking_dir = \"task\"\nenv_command = '{pwd}'\n"
        );
        let (config, _, evaluated) = load_in(dir.path(), &toml);
        let root = dir.path().canonicalize().unwrap();

        assert_eq!(
            get(&task_env(&config, &evaluated, 0), "CWD"),
            root.join("sub").to_str()
        );
        assert_eq!(
            get(&task_env(&config, &evaluated, 1), "CWD"),
            root.join("task").to_str()
        );
    }

    #[test]
    fn identical_runs_share_one_evaluation_and_a_different_input_runs_again() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("runs.log");
        let cmd = format!("echo run >> {}; printf '{{}}'", log.display());

        let toml = format!(
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = \"{cmd}\"\n\n\
             [[group]]\nname = \"b\"\npatterns = [\"*\"]\nenv_command = \"{cmd}\"\n\n\
             [[group]]\nname = \"c\"\npatterns = [\"*\"]\nenv = {{ DIFFERENT = \"1\" }}\n\n\
             [[group.task]]\ncommand = \"x\"\nenv_command = \"{cmd}\"\n",
        );
        load_in(dir.path(), &toml);

        let runs = std::fs::read_to_string(&log).unwrap();

        assert_eq!(runs.lines().count(), 2, "{runs}");
    }

    #[test]
    fn the_command_string_expands_variables() {
        let dir = tempfile::tempdir().unwrap();
        let (config, _, evaluated) = load_in(
            dir.path(),
            "[variables]\nvalue = \"expanded\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\n\
             env_command = \"printf '{\\\"V\\\":\\\"${value}\\\"}'\"\n\n\
             [[group.task]]\ncommand = \"x\"\n",
        );

        assert_eq!(
            get(&task_env(&config, &evaluated, 0), "V"),
            Some("expanded")
        );
    }

    #[test]
    fn a_failing_command_carries_its_stripped_stderr_and_stops_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let error = command_error(
            dir.path(),
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\n\
             env_command = \"printf '\\\\033[31mblocked\\\\033[0m\\\\n' >&2; exit 2\"\n",
        );

        assert_eq!(error.code(), "env_command_failed");
        assert_eq!(error.field, "group 'a' env_command");

        let text = error.to_string();
        assert!(text.contains("exited with status 2"), "{text}");
        assert!(text.contains("blocked"), "{text}");
        assert!(!text.contains('\x1b'), "{text}");
    }

    #[test]
    fn contract_violations_are_coded_and_never_echo_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let error = command_error(
            dir.path(),
            "[settings]\nenv_command = \"printf 'SECRET=hunter2'\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\n",
        );

        assert_eq!(error.code(), "env_command_malformed_output");
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }

    #[test]
    fn a_command_past_its_timeout_has_its_own_code() {
        let dir = tempfile::tempdir().unwrap();
        let error = command_error(
            dir.path(),
            "[settings]\nenv_command = \"sleep 30\"\nenv_command_timeout = \"300ms\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\n",
        );

        assert_eq!(error.code(), "env_command_timeout");
    }

    #[test]
    fn a_missing_working_directory_is_a_spawn_failure() {
        let dir = tempfile::tempdir().unwrap();
        let error = command_error(
            dir.path(),
            "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nworking_dir = \"nope\"\nenv_command = \"true\"\n",
        );

        assert_eq!(error.code(), "env_command_spawn_failed");
        assert!(error.to_string().contains("working directory"), "{error}");
    }

    #[test]
    fn a_cancelled_handle_fails_the_evaluation_before_any_command_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let (config, path) = config_in(
            dir.path(),
            &format!(
                "[settings]\nenv_command = \"touch {}; printf '{{}}'\"\n\n\
                 [[group]]\nname = \"a\"\npatterns = [\"*\"]\n",
                marker.display()
            ),
        );
        let cancel = CancelHandle::new();
        cancel.cancel();

        let error = evaluate(&config, &path, ENV_FILE_READ_DEADLINE, &cancel).unwrap_err();

        assert_eq!(error.code(), "evaluation_cancelled");
        assert!(!marker.exists());
    }

    #[test]
    fn provenance_records_keys_unsets_zero_key_sources_and_dropped_names() {
        std::env::set_var("ZAZ_UT_DROPPED_NAME", "1");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "").unwrap();
        let (_, _, evaluated) = load_in(
            dir.path(),
            "[settings]\nenv_file = \".env\"\n\
             env_command = \"printf '{\\\"K\\\":\\\"v\\\",\\\"GONE\\\":null}'\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\n",
        );

        let file = &evaluated.sources[0];
        assert_eq!(file.kind, SourceKind::File);
        assert!(file.set.is_empty() && file.unset.is_empty());

        let command = &evaluated.sources[1];
        assert_eq!(command.kind, SourceKind::Command);
        assert_eq!(command.set, ["K"]);
        assert_eq!(command.unset, ["GONE"]);

        assert_eq!(evaluated.dropped.len(), 1);
        assert!(evaluated.dropped[0]
            .names
            .iter()
            .any(|n| n == "ZAZ_UT_DROPPED_NAME"));
    }

    fn counted(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho run >> '{}'\n{body}\n",
                dir.join(format!("{name}.count")).display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        path.display().to_string()
    }

    fn runs(dir: &Path, name: &str) -> usize {
        std::fs::read_to_string(dir.join(format!("{name}.count")))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn changed(dir: &Path, name: &str) -> Scope {
        Scope::Partial(BTreeSet::from([zaz_watch::normalize(&dir.join(name))]))
    }

    fn refresh(
        config: &Config,
        path: &Path,
        scope: &Scope,
        cache: &mut EvalCache,
    ) -> Result<EvaluatedEnv, DaemonError> {
        evaluate_scoped(
            config,
            path,
            ENV_FILE_READ_DEADLINE,
            &CancelHandle::new(),
            scope,
            cache,
        )
    }

    #[test]
    fn scopes_union_and_a_full_run_absorbs_a_partial_one() {
        let a = Scope::Partial(BTreeSet::from([PathBuf::from("/a")]));
        let b = Scope::Partial(BTreeSet::from([PathBuf::from("/b")]));

        assert_eq!(
            a.clone().merge(b.clone()),
            Scope::Partial(BTreeSet::from([PathBuf::from("/a"), PathBuf::from("/b")]))
        );
        assert_eq!(a.clone().merge(Scope::Full), Scope::Full);
        assert_eq!(Scope::Full.merge(b), Scope::Full);
    }

    #[test]
    fn a_named_node_with_unchanged_content_is_gated_and_changed_content_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("watched"), "one").unwrap();
        let stub = counted(&root, "cmd", "printf '{\"V\":\"%s\"}' \"$(cat watched)\"");
        let (config, path) = config_in(
            &root,
            &format!(
                "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{stub}'\nenv_watch = [\"watched\"]\n"
            ),
        );
        let mut cache = EvalCache::default();
        let first = refresh(&config, &path, &Scope::Full, &mut cache).unwrap();
        assert_eq!(runs(&root, "cmd"), 1);

        std::fs::write(root.join("watched"), "one").unwrap();
        let same = refresh(&config, &path, &changed(&root, "watched"), &mut cache).unwrap();
        assert_eq!(
            runs(&root, "cmd"),
            1,
            "same content must not run the command"
        );
        assert_eq!(same.group("a"), first.group("a"));

        let other = refresh(&config, &path, &changed(&root, "unrelated"), &mut cache).unwrap();
        assert_eq!(
            runs(&root, "cmd"),
            1,
            "an event that names no node runs nothing"
        );
        assert_eq!(other.group("a"), first.group("a"));

        std::fs::write(root.join("watched"), "two").unwrap();
        let edited = refresh(&config, &path, &changed(&root, "watched"), &mut cache).unwrap();
        assert_eq!(runs(&root, "cmd"), 2);
        assert_ne!(edited.group("a"), first.group("a"));
    }

    #[test]
    fn an_absent_file_and_an_empty_file_hash_apart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stub = counted(&root, "cmd", "printf '{}'");
        let (config, path) = config_in(
            &root,
            &format!(
                "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{stub}'\nenv_watch = [\"watched\"]\n"
            ),
        );
        let mut cache = EvalCache::default();
        refresh(&config, &path, &Scope::Full, &mut cache).unwrap();

        std::fs::write(root.join("watched"), "").unwrap();
        refresh(&config, &path, &changed(&root, "watched"), &mut cache).unwrap();

        assert_eq!(runs(&root, "cmd"), 2);
    }

    #[test]
    fn a_directory_in_the_watch_list_always_counts_as_changed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("watched")).unwrap();
        let stub = counted(&root, "cmd", "printf '{}'");
        let (config, path) = config_in(
            &root,
            &format!(
                "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{stub}'\nenv_watch = [\"watched\"]\n"
            ),
        );
        let mut cache = EvalCache::default();
        refresh(&config, &path, &Scope::Full, &mut cache).unwrap();
        refresh(&config, &path, &changed(&root, "watched"), &mut cache).unwrap();

        assert_eq!(runs(&root, "cmd"), 2);
    }

    #[test]
    fn a_full_run_discards_the_nodes_and_runs_every_command() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stub = counted(&root, "cmd", "printf '{}'");
        let (config, path) = config_in(
            &root,
            &format!("[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{stub}'\n"),
        );
        let mut cache = EvalCache::default();

        refresh(&config, &path, &Scope::Full, &mut cache).unwrap();
        refresh(&config, &path, &Scope::Full, &mut cache).unwrap();

        assert_eq!(runs(&root, "cmd"), 2);
    }

    #[test]
    fn a_dirty_upstream_runs_the_child_and_a_gated_one_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("up.txt"), "one").unwrap();
        let upstream = counted(&root, "up", "printf '{\"UP\":\"%s\"}' \"$(cat up.txt)\"");
        let child = counted(&root, "child", "printf '{\"V\":\"saw-%s\"}' \"$UP\"");
        let (config, path) = config_in(
            &root,
            &format!(
                "[settings]\nenv_command = '{upstream}'\nenv_watch = [\"up.txt\"]\n\n\
                 [[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{child}'\n"
            ),
        );
        let mut cache = EvalCache::default();
        refresh(&config, &path, &Scope::Full, &mut cache).unwrap();

        std::fs::write(root.join("up.txt"), "one").unwrap();
        refresh(&config, &path, &changed(&root, "up.txt"), &mut cache).unwrap();
        assert_eq!((runs(&root, "up"), runs(&root, "child")), (1, 1));

        std::fs::write(root.join("up.txt"), "two").unwrap();
        refresh(&config, &path, &changed(&root, "up.txt"), &mut cache).unwrap();
        assert_eq!((runs(&root, "up"), runs(&root, "child")), (2, 2));
    }

    #[test]
    fn a_failed_node_is_never_gated_and_its_upstream_stays_cached() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("up.txt"), "one").unwrap();
        let fail = root.join("fail");
        let upstream = counted(&root, "up", "printf '{\"UP\":\"%s\"}' \"$(cat up.txt)\"");
        let child = counted(
            &root,
            "child",
            &format!(
                "if [ -e '{}' ]; then exit 3; fi\nprintf '{{\"V\":\"saw-%s\"}}' \"$UP\"",
                fail.display()
            ),
        );
        let (config, path) = config_in(
            &root,
            &format!(
                "[settings]\nenv_command = '{upstream}'\nenv_watch = [\"up.txt\"]\n\n\
                 [[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{child}'\n"
            ),
        );
        let mut cache = EvalCache::default();
        refresh(&config, &path, &Scope::Full, &mut cache).unwrap();

        std::fs::write(&fail, "").unwrap();
        std::fs::write(root.join("up.txt"), "two").unwrap();
        let error = refresh(&config, &path, &changed(&root, "up.txt"), &mut cache).unwrap_err();
        assert_eq!(error.code(), "env_command_failed");
        assert_eq!((runs(&root, "up"), runs(&root, "child")), (2, 2));

        std::fs::remove_file(&fail).unwrap();
        let retried = refresh(&config, &path, &changed(&root, "up.txt"), &mut cache).unwrap();
        assert_eq!((runs(&root, "up"), runs(&root, "child")), (2, 3));
        assert!(retried.group("a").is_some());
    }

    #[test]
    fn levels_that_share_a_command_all_refresh_when_one_of_them_lists_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("w"), "one").unwrap();
        let stub = counted(&root, "cmd", "printf '{\"V\":\"%s\"}' \"$(cat w)\"");
        let (config, path) = config_in(
            &root,
            &format!(
                "[[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{stub}'\nenv_watch = [\"w\"]\n\n\
                 [[group]]\nname = \"b\"\npatterns = [\"*\"]\nenv_command = '{stub}'\n"
            ),
        );
        let mut cache = EvalCache::default();
        let first = refresh(&config, &path, &Scope::Full, &mut cache).unwrap();
        assert_eq!(runs(&root, "cmd"), 1);

        std::fs::write(root.join("w"), "two").unwrap();
        let after = refresh(&config, &path, &changed(&root, "w"), &mut cache).unwrap();

        assert_eq!(runs(&root, "cmd"), 2);
        assert_ne!(first.group("a"), after.group("a"));
        assert_ne!(first.group("b"), after.group("b"));
    }

    #[test]
    fn an_env_file_save_touch_or_comment_edit_runs_no_downstream_command_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join(".env"), "A=1\n").unwrap();
        let stub = counted(&root, "cmd", "printf '{\"V\":\"%s\"}' \"$A\"");
        let (config, path) = config_in(
            &root,
            &format!(
                "[settings]\nenv_file = \".env\"\n\n\
                 [[group]]\nname = \"a\"\npatterns = [\"*\"]\nenv_command = '{stub}'\n"
            ),
        );
        let mut cache = EvalCache::default();
        let first = refresh(&config, &path, &Scope::Full, &mut cache).unwrap();

        for text in ["A=1\n", "A=1\n", "# only a comment\nA=1\n"] {
            std::fs::write(root.join(".env"), text).unwrap();
            let after = refresh(&config, &path, &changed(&root, ".env"), &mut cache).unwrap();

            assert_eq!(after.group("a"), first.group("a"));
        }
        assert_eq!(runs(&root, "cmd"), 1);

        std::fs::write(root.join(".env"), "A=2\n").unwrap();
        let edited = refresh(&config, &path, &changed(&root, ".env"), &mut cache).unwrap();

        assert_eq!(runs(&root, "cmd"), 2);
        assert_ne!(edited.group("a"), first.group("a"));
    }

    const REPORT_CONFIG: &str = r#"
[settings]
env_file = ".env"

[[group]]
name = "a"
patterns = ["*"]
env_command = "cat group.json"
env = { GI = "zz-val-inline" }

[[group.task]]
name = "t"
command = "true"
env_file = "task.env"
env = { TI = "zz-val-task-inline" }

[[group.service]]
name = "s"
command = "sleep 1"
env_command = "printf ''"

[[group]]
name = "b"
patterns = ["*"]

[[group.task]]
name = "plain"
command = "true"
"#;

    fn report_of(dir: &Path, toml: &str) -> EnvReport {
        std::fs::write(
            dir.join(".env"),
            "S=zz-val-settings\nS2=zz-val-settings-2\n",
        )
        .unwrap();
        std::fs::write(dir.join("task.env"), "T=zz-val-task\n").unwrap();
        std::fs::write(dir.join("group.json"), r#"{"G":"zz-val-group","S":null}"#).unwrap();

        let (config, _path, evaluated) = load_in(dir, toml);

        evaluated.report(&config, DaemonStatus::Running, None)
    }

    fn fields(sources: &[EnvSourceReport]) -> Vec<&str> {
        sources.iter().map(|source| source.field.as_str()).collect()
    }

    #[test]
    fn the_report_lists_each_level_in_the_order_it_applies() {
        let dir = tempfile::tempdir().unwrap();
        let report = report_of(dir.path(), REPORT_CONFIG);

        let a = &report.groups[0];
        assert_eq!(
            fields(&a.sources),
            [
                "settings env_file",
                "group 'a' env_command",
                "group 'a' env"
            ]
        );
        assert_eq!(a.starts_from, EnvStart::Base);

        let file = &a.sources[0];
        assert_eq!(file.level, EnvLevel::Settings);
        assert_eq!(file.kind, EnvSourceKind::File);
        assert_eq!(file.origin.as_deref(), Some(".env"));
        assert_eq!(file.set, ["S", "S2"]);
        assert!(!file.produced_no_keys);

        let command = &a.sources[1];
        assert_eq!(command.level, EnvLevel::Group);
        assert_eq!(command.kind, EnvSourceKind::Command);
        assert_eq!(command.origin.as_deref(), Some("cat group.json"));
        assert_eq!(command.set, ["G"]);
        assert_eq!(command.unset, ["S"]);

        let inline = &a.sources[2];
        assert_eq!(inline.kind, EnvSourceKind::Inline);
        assert_eq!(inline.origin, None);
        assert_eq!(inline.set, ["GI"]);
    }

    #[test]
    fn a_task_and_a_service_add_their_own_sources_after_the_group_chain() {
        let dir = tempfile::tempdir().unwrap();
        let report = report_of(dir.path(), REPORT_CONFIG);
        let a = &report.groups[0];

        assert_eq!(
            fields(&a.tasks[0].sources),
            [
                "settings env_file",
                "group 'a' env_command",
                "group 'a' env",
                "group 'a' task 't' env_file",
                "group 'a' task 't' env",
            ]
        );
        assert_eq!(a.tasks[0].name, "t");
        assert_eq!(a.tasks[0].sources[3].level, EnvLevel::Task);
        assert_eq!(a.tasks[0].sources[3].set, ["T"]);
        assert_eq!(a.tasks[0].sources[4].set, ["TI"]);

        let service = &a.services[0];
        let last = service.sources.last().unwrap();
        assert_eq!(last.level, EnvLevel::Service);
        assert_eq!(last.field, "group 'a' service 's' env_command");
        assert_eq!(service.starts_from, EnvStart::Base);
    }

    #[test]
    fn a_command_that_prints_nothing_is_flagged_as_producing_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        let report = report_of(dir.path(), REPORT_CONFIG);
        let service = &report.groups[0].services[0];
        let silent = service.sources.last().unwrap();

        assert!(silent.produced_no_keys);
        assert!(silent.set.is_empty() && silent.unset.is_empty());

        let loaded = &report.groups[0].tasks[0].sources;
        assert!(loaded.iter().filter(|s| s.produced_no_keys).count() == 0);
    }

    #[test]
    fn a_group_with_no_command_starts_from_the_daemons_environment() {
        let dir = tempfile::tempdir().unwrap();
        let report = report_of(dir.path(), REPORT_CONFIG);
        let b = &report.groups[1];

        assert_eq!(b.starts_from, EnvStart::Daemon);
        assert_eq!(b.tasks[0].starts_from, EnvStart::Daemon);
        assert_eq!(fields(&b.sources), ["settings env_file"]);
    }

    #[test]
    fn a_settings_command_puts_every_group_on_the_base_and_reports_the_dropped_names() {
        let dir = tempfile::tempdir().unwrap();
        let report = report_of(
            dir.path(),
            "[settings]\nenv_command = \"printf '{}'\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\n",
        );

        assert_eq!(report.groups[0].starts_from, EnvStart::Base);
        assert!(report.groups[0].sources[0].produced_no_keys);
        assert_eq!(
            report.dropped_inherited,
            env::dropped_names(&Passthrough::default())
        );
    }

    #[test]
    fn the_report_never_carries_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let report = report_of(dir.path(), REPORT_CONFIG);
        let json = serde_json::to_string(&report).unwrap();

        assert!(json.contains("\"S2\""));
        assert!(!json.contains("zz-val"));
    }
}
