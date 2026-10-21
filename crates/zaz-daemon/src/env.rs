//! Process environment construction.
//!
//! Every process's complete environment is built here from a starting environment and the raw,
//! ordered layers of its chain. Group build and every task spawn call the same functions, so the
//! layering cannot drift between spawn sites.
//!
//! A process's environment is never derived from a parent executor's result. A level that selects
//! a different starting environment needs the whole chain rebuilt from the raw layers.

use std::collections::HashMap;
use std::path::Path;
use zaz_process::Executor;

/// One source's changes to an environment.
///
/// A `Some` value sets the variable. A `None` value unsets it.
#[derive(Debug, Clone, Default)]
pub(crate) struct EnvLayer(HashMap<String, Option<String>>);

impl EnvLayer {
    /// A layer that sets every entry of an inline `env` table.
    pub(crate) fn from_inline(env: &HashMap<String, String>) -> Self {
        Self(
            env.iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect(),
        )
    }

    fn apply(&self, env: &mut HashMap<String, String>) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => env.insert(key.clone(), value.clone()),
                None => env.remove(key),
            };
        }
    }
}

/// The daemon's own environment.
pub(crate) fn inherited_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// Build a complete environment from a starting environment and ordered layers.
///
/// Layers apply in order, so a later layer wins, and an unset removes the key at that point.
pub(crate) fn build_env(
    start: HashMap<String, String>,
    layers: &[EnvLayer],
) -> HashMap<String, String> {
    let mut env = start;

    for layer in layers {
        layer.apply(&mut env);
    }

    env
}

/// The complete environment of a process in a group.
///
/// The chain is group, then process. The start is always the inherited environment today.
fn process_env(
    group_env: &HashMap<String, String>,
    process_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    build_env(
        inherited_env(),
        &[
            EnvLayer::from_inline(group_env),
            EnvLayer::from_inline(process_env),
        ],
    )
}

/// Build the executor for a group.
///
/// A relative `working_dir` resolves against the config directory, which is also the default.
pub(crate) fn group_executor(
    shell: Option<String>,
    config_dir: &Path,
    working_dir: Option<&str>,
    group_env: &HashMap<String, String>,
) -> Executor {
    let env = build_env(inherited_env(), &[EnvLayer::from_inline(group_env)]);
    let executor = Executor::new(shell)
        .with_working_dir(config_dir.to_string_lossy().to_string())
        .with_env(env);

    with_resolved_working_dir(executor, config_dir, working_dir)
}

/// Build the executor for a task or service in a group.
///
/// The shell and working directory come from the group's executor. The environment is rebuilt
/// from the raw group and process layers. A `working_dir` overrides the group's, resolved against
/// the config directory when relative.
pub(crate) fn process_executor(
    group: &Executor,
    config_dir: &Path,
    working_dir: Option<&str>,
    group_env: &HashMap<String, String>,
    env: &HashMap<String, String>,
) -> Executor {
    let executor = group.clone().with_env(process_env(group_env, env));

    with_resolved_working_dir(executor, config_dir, working_dir)
}

fn with_resolved_working_dir(
    executor: Executor,
    config_dir: &Path,
    working_dir: Option<&str>,
) -> Executor {
    match working_dir {
        Some(dir) => {
            let resolved = zaz_config::resolve_against_config_dir(config_dir, dir);
            executor.with_working_dir(resolved.to_string_lossy().to_string())
        }
        None => executor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn later_layers_win() {
        let start = map(&[("A", "start"), ("B", "start"), ("C", "start")]);
        let group = EnvLayer::from_inline(&map(&[("B", "group"), ("C", "group")]));
        let process = EnvLayer::from_inline(&map(&[("C", "process")]));

        let env = build_env(start, &[group, process]);

        assert_eq!(env["A"], "start");
        assert_eq!(env["B"], "group");
        assert_eq!(env["C"], "process");
    }

    #[test]
    fn unset_removes_the_key_and_a_later_set_restores_it() {
        let start = map(&[("A", "start"), ("B", "start")]);
        let unset = EnvLayer(
            [("A".to_string(), None), ("B".to_string(), None)]
                .into_iter()
                .collect(),
        );
        let reset = EnvLayer::from_inline(&map(&[("B", "again")]));

        let env = build_env(start, &[unset, reset]);

        assert!(!env.contains_key("A"));
        assert_eq!(env["B"], "again");
    }

    #[test]
    fn task_env_beats_group_env_beats_inherited() {
        std::env::set_var("ZAZ_TEST_MERGE_INHERITED", "inherited");
        std::env::set_var("ZAZ_TEST_MERGE_SHADOWED", "inherited");
        std::env::set_var("ZAZ_TEST_MERGE_OVERRIDDEN", "inherited");

        let group_env = map(&[
            ("ZAZ_TEST_MERGE_SHADOWED", "group"),
            ("ZAZ_TEST_MERGE_OVERRIDDEN", "group"),
        ]);
        let task_env = map(&[("ZAZ_TEST_MERGE_OVERRIDDEN", "task")]);

        let group = group_executor(None, Path::new("/cfg"), None, &group_env);
        let task = process_executor(&group, Path::new("/cfg"), None, &group_env, &task_env);

        assert_eq!(task.env()["ZAZ_TEST_MERGE_INHERITED"], "inherited");
        assert_eq!(task.env()["ZAZ_TEST_MERGE_SHADOWED"], "group");
        assert_eq!(task.env()["ZAZ_TEST_MERGE_OVERRIDDEN"], "task");
        assert_eq!(group.env()["ZAZ_TEST_MERGE_OVERRIDDEN"], "group");
    }

    #[test]
    fn working_dir_defaults_to_config_dir_and_resolves_relative() {
        let config_dir = Path::new("/cfg");
        let empty = HashMap::new();

        let group = group_executor(None, config_dir, None, &empty);
        let relative = group_executor(None, config_dir, Some("sub"), &empty);
        let absolute = group_executor(None, config_dir, Some("/abs"), &empty);

        assert_eq!(group.working_dir(), Some("/cfg"));
        assert_eq!(relative.working_dir(), Some("/cfg/sub"));
        assert_eq!(absolute.working_dir(), Some("/abs"));

        let task = process_executor(&relative, config_dir, Some("task"), &empty, &empty);
        assert_eq!(task.working_dir(), Some("/cfg/task"));

        let inherited = process_executor(&relative, config_dir, None, &empty, &empty);
        assert_eq!(inherited.working_dir(), Some("/cfg/sub"));
    }
}
