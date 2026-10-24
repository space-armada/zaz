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

use crate::env::{self, EnvLayer, EnvMap, GroupEnv, Passthrough, Start};
use crate::env_command::{self, EnvCommandError, EnvCommandErrorKind};
use crate::DaemonError;
use std::collections::HashMap;
use std::ffi::OsString;
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

/// Load the config at `config_path` and evaluate the sources it names.
///
/// `config_path` must be absolute. This blocks on file reads, each for at most `read_deadline`, and
/// on commands, each under its own timeout. `cancel` stops the command that is running.
pub(crate) fn prepare(
    config_path: &Path,
    read_deadline: Duration,
    cancel: &CancelHandle,
) -> Result<Prepared, DaemonError> {
    let config = zaz_config::load(config_path).map_err(DaemonError::Config)?;
    let evaluated = evaluate(&config, config_path, read_deadline, cancel)?;

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

/// Run `work` on its own detached thread and deliver its result to `done`.
///
/// The thread is detached, not a runtime blocking task, so the runtime can shut down while it
/// runs. `work` bounds its own blocking reads and commands, so the thread always finishes.
///
/// `done` runs exactly once. A panic in `work`, or a thread that cannot start, reaches it as an
/// error.
pub(crate) fn run_detached<W, F>(work: W, done: F)
where
    W: FnOnce() -> Result<Prepared, DaemonError> + Send + 'static,
    F: FnOnce(Result<Prepared, DaemonError>) + Send + 'static,
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
    field: String,
    kind: SourceKind,
    set: Vec<String>,
    unset: Vec<String>,
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
                field: source.field.clone(),
                kind: SourceKind::File,
                set: layer.set_keys(),
                unset: Vec::new(),
            });
            files.insert(source.level, layer);
        }
    }

    let mut run = CommandRun::new(config, config_path, cancel);
    let mut commands: HashMap<EnvFileLevel, EnvLayer> = HashMap::new();
    let mut dropped = Vec::new();

    for source in run.ordered_sources() {
        let input = run.input_env(&source.level, &files, &commands);
        let layer = run.execute(&source, input)?;

        sources.push(SourceRecord {
            field: source.field.clone(),
            kind: SourceKind::Command,
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

    log_provenance(&evaluated);

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
    config: &'a Config,
    config_path: &'a Path,
    config_dir: PathBuf,
    cancel: &'a CancelHandle,
    shell: String,
    timeout: Duration,
    passthrough: Passthrough,
    context: zaz_vars::Context,
    cache: HashMap<CacheKey, EnvLayer>,
}

impl<'a> CommandRun<'a> {
    fn new(config: &'a Config, config_path: &'a Path, cancel: &'a CancelHandle) -> Self {
        let config_dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        Self {
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
            cache: HashMap::new(),
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

    /// Run one command, or reuse the output of an identical run in this load.
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

        if let Some(layer) = self.cache.get(&key) {
            tracing::debug!(field = %source.field, "reusing the output of an identical env_command");
            return Ok(layer.clone());
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
        )
        .map_err(&fail)?;

        let layer = EnvLayer::from_output(output);
        self.cache.insert(key, layer.clone());

        Ok(layer)
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
}
