//! The evaluation step.
//!
//! `zaz_config::load` only parses and validates. Reading the sources a config names happens here,
//! once per load or reload, and produces the environment every group is built from. Group build
//! takes an [`EvaluatedEnv`] as a required input, so no group can be built without one.
//!
//! [`evaluate`] is a plain function of the config and the config path. It blocks on file reads, and
//! a caller that must not block runs it on a blocking thread.

use crate::env::{EnvLayer, GroupEnv};
use crate::DaemonError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zaz_config::{Config, EnvFileError, EnvFileLevel, SpecialFiles};

/// A config loaded from disk together with everything its sources evaluated to.
///
/// This is all the slow work of a load or reload. Building groups from it is quick and runs on
/// the daemon's event loop.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub(crate) config: Config,
    pub(crate) evaluated: EvaluatedEnv,
}

/// Load the config at `config_path` and evaluate the sources it names.
///
/// `config_path` must be absolute. This blocks on file reads.
pub(crate) fn prepare(config_path: &Path) -> Result<Prepared, DaemonError> {
    let config = zaz_config::load(config_path).map_err(DaemonError::Config)?;
    let evaluated = evaluate(&config, config_path).map_err(DaemonError::EnvFile)?;

    Ok(Prepared { config, evaluated })
}

/// How long one load and evaluation may run before the run is abandoned.
///
/// A source that never finishes, such as a named pipe with no writer, would otherwise hold the
/// run in flight for good, and every later reload would queue behind it.
pub(crate) const LOAD_DEADLINE: Duration = Duration::from_secs(120);

/// Run [`prepare`] on its own thread and deliver the result to `done`.
///
/// See [`run_detached`] for the thread, the deadline, and the panic handling.
pub(crate) fn prepare_detached<F>(config_path: PathBuf, deadline: Duration, done: F)
where
    F: FnOnce(Result<Prepared, DaemonError>) + Send + 'static,
{
    run_detached(deadline, move || prepare(&config_path), done);
}

/// Run `work` on its own detached thread and deliver its result to `done`.
///
/// The thread is detached, not a runtime blocking task. A read that never returns would otherwise
/// keep the runtime from shutting down. A thread blocked in a read cannot be killed, so a run that
/// outlasts `deadline` is abandoned instead. `done` then gets [`DaemonError::LoadTimeout`], and the
/// thread's own result, if it ever comes, is dropped.
///
/// `done` runs exactly once. A panic in `work`, or a thread that cannot start, reaches it as an
/// error.
pub(crate) fn run_detached<W, F>(deadline: Duration, work: W, done: F)
where
    W: FnOnce() -> Result<Prepared, DaemonError> + Send + 'static,
    F: FnOnce(Result<Prepared, DaemonError>) + Send + 'static,
{
    // Whichever of the worker, the watchdog, or a failed spawn takes `done` from the slot first
    // delivers the result. The others find it empty.
    let slot = Arc::new(Mutex::new(Some(done)));
    let worker_slot = Arc::clone(&slot);

    // Dropping this sender, however the worker ends, tells the watchdog to stand down.
    let (finished_tx, finished_rx) = std::sync::mpsc::channel::<()>();

    let spawned = std::thread::Builder::new()
        .name("zaz-evaluate".to_string())
        .spawn(move || {
            let _finished = finished_tx;
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
        return;
    }

    let watchdog = std::thread::Builder::new()
        .name("zaz-evaluate-deadline".to_string())
        .spawn(move || {
            if finished_rx.recv_timeout(deadline) != Err(RecvTimeoutError::Timeout) {
                return;
            }

            if let Some(done) = take_done(&slot) {
                tracing::error!(limit = ?deadline, "abandoning a load that did not finish");
                done(Err(DaemonError::LoadTimeout { limit: deadline }));
            }
        });

    if let Err(e) = watchdog {
        tracing::error!(error = %e, "failed to start the load deadline thread, so the load has no deadline");
    }
}

fn take_done<F>(slot: &Mutex<Option<F>>) -> Option<F> {
    slot.lock().ok().and_then(|mut slot| slot.take())
}

/// The environment layers every group of a config is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EvaluatedEnv {
    groups: HashMap<String, GroupEnv>,
}

impl EvaluatedEnv {
    /// The evaluated layers of a group, by name.
    pub(crate) fn group(&self, name: &str) -> Option<&GroupEnv> {
        self.groups.get(name)
    }
}

/// Read every source `config` names and build the layers of each group.
///
/// `config_path` must be absolute. A relative `env_file` resolves against its directory. Each
/// distinct file is read once, so every level that names it sees the same bytes. A failing
/// file fails the evaluation. Every file is still read, so all errors are reported.
pub(crate) fn evaluate(
    config: &Config,
    config_path: &Path,
) -> Result<EvaluatedEnv, Vec<EnvFileError>> {
    let reads =
        zaz_config::read_env_files(config, config_path, SpecialFiles::Read, |field, path| {
            tracing::info!(
                config = %config_path.display(),
                field = %field,
                path = %path.display(),
                "loading env_file"
            );
        });

    if !reads.errors.is_empty() {
        return Err(reads.errors);
    }

    let mut levels: HashMap<EnvFileLevel, EnvLayer> = HashMap::new();

    for source in reads.sources() {
        if let Some(vars) = reads.vars(source) {
            levels.insert(source.level, EnvLayer::from_vars(vars));
        }
    }

    let mut groups = HashMap::new();

    for (g, group) in config.groups.iter().enumerate() {
        let mut chain = Vec::new();
        chain.extend(levels.get(&EnvFileLevel::Settings).cloned());
        chain.extend(levels.get(&EnvFileLevel::Group { group: g }).cloned());
        chain.push(EnvLayer::from_vars(&group.env));

        let tasks = group
            .tasks
            .iter()
            .enumerate()
            .map(|(t, task)| {
                process_layers(
                    levels.get(&EnvFileLevel::Task { group: g, task: t }),
                    &task.env,
                )
            })
            .collect();

        let services = group
            .services
            .iter()
            .enumerate()
            .map(|(s, service)| {
                process_layers(
                    levels.get(&EnvFileLevel::Service {
                        group: g,
                        service: s,
                    }),
                    &service.env,
                )
            })
            .collect();

        groups.insert(group.name.clone(), GroupEnv::new(chain, tasks, services));
    }

    Ok(EvaluatedEnv { groups })
}

fn process_layers(file: Option<&EnvLayer>, inline: &HashMap<String, String>) -> Vec<EnvLayer> {
    let mut layers: Vec<EnvLayer> = file.cloned().into_iter().collect();
    layers.push(EnvLayer::from_vars(inline));

    layers
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let evaluated = evaluate(&config, &path).unwrap();

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
        let before = evaluate(&config, &path).unwrap();

        std::fs::write(dir.path().join(".env"), "A=2\n").unwrap();
        let after = evaluate(&config, &path).unwrap();

        assert_ne!(before, after);

        std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
        assert_eq!(before, evaluate(&config, &path).unwrap());
    }

    #[test]
    fn a_missing_file_fails_the_evaluation_with_its_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let (config, path) = config_in(
            dir.path(),
            "[settings]\nenv_file = \"nope.env\"\n\n[[group]]\nname = \"a\"\npatterns = [\"*\"]\n",
        );

        let err = evaluate(&config, &path).unwrap_err().remove(0);

        assert_eq!(err.code(), "env_file_missing");
        assert_eq!(err.path, dir.path().join("nope.env"));
        assert_eq!(err.config, path);
        assert_eq!(err.field, "settings env_file");
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
        let before = evaluate(&config, &path).unwrap();

        std::fs::write(dir.path().join(".env"), "A=2\n").unwrap();
        let after = evaluate(&config, &path).unwrap();

        assert_ne!(before.group("a"), after.group("a"));
        assert_ne!(before.group("b"), after.group("b"));
    }
}
