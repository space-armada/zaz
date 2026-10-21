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
use zaz_config::{Config, EnvFileError, EnvFileLevel};

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

/// Run [`prepare`] on its own thread and deliver the result to `done`.
///
/// The thread is detached, not a runtime blocking task. A read that never returns, such as one on
/// a named pipe with no writer, would otherwise keep the runtime from shutting down. A panic
/// becomes an error instead of a silent drop.
pub(crate) fn prepare_detached<F>(config_path: PathBuf, done: F)
where
    F: FnOnce(Result<Prepared, DaemonError>) + Send + 'static,
{
    run_detached(move || prepare(&config_path), done);
}

/// Run `work` on its own detached thread and deliver its result to `done`.
///
/// `done` always runs once. A panic in `work`, or a thread that cannot start, reaches it as an
/// error.
pub(crate) fn run_detached<W, F>(work: W, done: F)
where
    W: FnOnce() -> Result<Prepared, DaemonError> + Send + 'static,
    F: FnOnce(Result<Prepared, DaemonError>) + Send + 'static,
{
    // The thread owns `done` once it starts. A failed spawn drops the closure, so `done` goes
    // through a shared slot that either side can take.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(done)));
    let thread_slot = std::sync::Arc::clone(&slot);

    let spawned = std::thread::Builder::new()
        .name("zaz-evaluate".to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or(Err(DaemonError::EvaluationPanicked));
            let done = thread_slot.lock().ok().and_then(|mut slot| slot.take());

            if let Some(done) = done {
                done(result);
            }
        });

    if let Err(e) = spawned {
        tracing::error!(error = %e, "failed to start the evaluation thread");

        let done = slot.lock().ok().and_then(|mut slot| slot.take());
        if let Some(done) = done {
            done(Err(DaemonError::EvaluationThread(e)));
        }
    }
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
    let config_dir = config_path.parent().unwrap_or(Path::new("."));

    let mut errors = Vec::new();
    let mut read: HashMap<PathBuf, EnvLayer> = HashMap::new();
    let mut levels: HashMap<EnvFileLevel, EnvLayer> = HashMap::new();

    for source in zaz_config::env_file_sources(config, config_dir) {
        tracing::info!(
            config = %config_path.display(),
            field = %source.field,
            path = %source.path.display(),
            "loading env_file"
        );

        let layer = match read.get(&source.path) {
            Some(layer) => layer.clone(),
            None => match zaz_config::read_env_file(config_path, &source) {
                Ok(vars) => {
                    let layer = EnvLayer::from_vars(&vars);
                    read.insert(source.path.clone(), layer.clone());
                    layer
                }
                Err(found) => {
                    errors.extend(found);
                    continue;
                }
            },
        };

        levels.insert(source.level, layer);
    }

    if !errors.is_empty() {
        return Err(errors);
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
