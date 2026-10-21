//! Process environment construction.
//!
//! Every process's complete environment is built here from a starting environment and the raw,
//! ordered layers of its chain. Group build and every task spawn call the same functions, so the
//! layering cannot drift between spawn sites.
//!
//! A process's environment is never derived from a parent executor's result. A level that selects
//! a different starting environment needs the whole chain rebuilt from the raw layers.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use zaz_process::Executor;

/// One source's changes to an environment.
///
/// A `Some` value sets the variable. A `None` value unsets it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnvLayer(BTreeMap<String, Option<String>>);

impl EnvLayer {
    /// A layer that sets every entry of an inline `env` table or a parsed `env_file`.
    pub(crate) fn from_vars<'a>(vars: impl IntoIterator<Item = (&'a String, &'a String)>) -> Self {
        Self(
            vars.into_iter()
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

/// The ordered layers a group contributes, with each task's and service's own layers.
///
/// This is the evaluated environment of one group. It holds loaded `env_file` values, so an edited
/// file makes the value compare unequal and the reload diff rebuilds the group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GroupEnv {
    group: Vec<EnvLayer>,
    tasks: Vec<Vec<EnvLayer>>,
    services: Vec<Vec<EnvLayer>>,
}

impl GroupEnv {
    pub(crate) fn new(
        group: Vec<EnvLayer>,
        tasks: Vec<Vec<EnvLayer>>,
        services: Vec<Vec<EnvLayer>>,
    ) -> Self {
        Self {
            group,
            tasks,
            services,
        }
    }

    fn process(&self, process: ProcessRef) -> impl Iterator<Item = &EnvLayer> {
        let own = match process {
            ProcessRef::Task(index) => self.tasks.get(index),
            ProcessRef::Service(index) => self.services.get(index),
        };

        self.group.iter().chain(own.into_iter().flatten())
    }
}

/// A task or service of a group, by its position in the group's config.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProcessRef {
    Task(usize),
    Service(usize),
}

/// The daemon's own environment.
pub(crate) fn inherited_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// Build a complete environment from a starting environment and ordered layers.
///
/// Layers apply in order, so a later layer wins, and an unset removes the key at that point.
pub(crate) fn build_env<'a>(
    start: HashMap<String, String>,
    layers: impl IntoIterator<Item = &'a EnvLayer>,
) -> HashMap<String, String> {
    let mut env = start;

    for layer in layers {
        layer.apply(&mut env);
    }

    env
}

/// Build the executor for a group.
///
/// A relative `working_dir` resolves against the config directory, which is also the default.
pub(crate) fn group_executor(
    shell: Option<String>,
    config_dir: &Path,
    working_dir: Option<&str>,
    env: &GroupEnv,
) -> Executor {
    let executor = Executor::new(shell)
        .with_working_dir(config_dir.to_string_lossy().to_string())
        .with_env(build_env(inherited_env(), &env.group));

    with_resolved_working_dir(executor, config_dir, working_dir)
}

/// Build the executor for a task or service in a group.
///
/// The shell and working directory come from the group's executor. The environment is rebuilt
/// from the raw layers of the whole chain. A `working_dir` overrides the group's, resolved against
/// the config directory when relative.
pub(crate) fn process_executor(
    group: &Executor,
    config_dir: &Path,
    working_dir: Option<&str>,
    env: &GroupEnv,
    process: ProcessRef,
) -> Executor {
    let executor = group
        .clone()
        .with_env(build_env(inherited_env(), env.process(process)));

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

    fn file(pairs: &[(&str, &str)]) -> EnvLayer {
        EnvLayer::from_vars(&map(pairs))
    }

    fn unset(keys: &[&str]) -> EnvLayer {
        EnvLayer(keys.iter().map(|k| (k.to_string(), None)).collect())
    }

    #[test]
    fn later_layers_win() {
        let start = map(&[("A", "start"), ("B", "start"), ("C", "start")]);
        let group = EnvLayer::from_vars(&map(&[("B", "group"), ("C", "group")]));
        let process = EnvLayer::from_vars(&map(&[("C", "process")]));

        let env = build_env(start, &[group, process]);

        assert_eq!(env["A"], "start");
        assert_eq!(env["B"], "group");
        assert_eq!(env["C"], "process");
    }

    #[test]
    fn unset_removes_the_key_and_a_later_set_restores_it() {
        let start = map(&[("A", "start"), ("B", "start")]);
        let reset = EnvLayer::from_vars(&map(&[("B", "again")]));

        let env = build_env(start, &[unset(&["A", "B"]), reset]);

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
        let env = GroupEnv::new(
            vec![EnvLayer::from_vars(&group_env)],
            vec![vec![EnvLayer::from_vars(&task_env)]],
            Vec::new(),
        );

        let group = group_executor(None, Path::new("/cfg"), None, &env);
        let task = process_executor(&group, Path::new("/cfg"), None, &env, ProcessRef::Task(0));

        assert_eq!(task.env()["ZAZ_TEST_MERGE_INHERITED"], "inherited");
        assert_eq!(task.env()["ZAZ_TEST_MERGE_SHADOWED"], "group");
        assert_eq!(task.env()["ZAZ_TEST_MERGE_OVERRIDDEN"], "task");
        assert_eq!(group.env()["ZAZ_TEST_MERGE_OVERRIDDEN"], "group");
    }

    #[test]
    fn files_sit_below_inline_env_at_each_level_and_levels_stack() {
        let env = GroupEnv::new(
            vec![
                file(&[
                    ("A", "settings-file"),
                    ("B", "settings-file"),
                    ("C", "settings-file"),
                ]),
                file(&[
                    ("B", "group-file"),
                    ("C", "group-file"),
                    ("D", "group-file"),
                ]),
                EnvLayer::from_vars(&map(&[("C", "group-inline")])),
            ],
            vec![vec![
                file(&[("D", "task-file"), ("E", "task-file")]),
                EnvLayer::from_vars(&map(&[("E", "task-inline")])),
            ]],
            Vec::new(),
        );

        let group = group_executor(None, Path::new("/cfg"), None, &env);
        let task = process_executor(&group, Path::new("/cfg"), None, &env, ProcessRef::Task(0));

        assert_eq!(group.env()["A"], "settings-file");
        assert_eq!(group.env()["B"], "group-file");
        assert_eq!(group.env()["C"], "group-inline");
        assert_eq!(group.env()["D"], "group-file");
        assert!(!group.env().contains_key("E"));

        assert_eq!(task.env()["C"], "group-inline");
        assert_eq!(task.env()["D"], "task-file");
        assert_eq!(task.env()["E"], "task-inline");
    }

    #[test]
    fn a_process_sees_only_its_own_layers() {
        let env = GroupEnv::new(
            Vec::new(),
            vec![vec![file(&[("T", "0")])], vec![file(&[("T", "1")])]],
            vec![vec![file(&[("S", "0")])]],
        );

        let group = group_executor(None, Path::new("/cfg"), None, &env);
        let task1 = process_executor(&group, Path::new("/cfg"), None, &env, ProcessRef::Task(1));
        let service0 = process_executor(
            &group,
            Path::new("/cfg"),
            None,
            &env,
            ProcessRef::Service(0),
        );

        assert_eq!(task1.env()["T"], "1");
        assert!(!task1.env().contains_key("S"));
        assert_eq!(service0.env()["S"], "0");
        assert!(!service0.env().contains_key("T"));
    }

    #[test]
    fn working_dir_defaults_to_config_dir_and_resolves_relative() {
        let config_dir = Path::new("/cfg");
        let env = GroupEnv::default();

        let group = group_executor(None, config_dir, None, &env);
        let relative = group_executor(None, config_dir, Some("sub"), &env);
        let absolute = group_executor(None, config_dir, Some("/abs"), &env);

        assert_eq!(group.working_dir(), Some("/cfg"));
        assert_eq!(relative.working_dir(), Some("/cfg/sub"));
        assert_eq!(absolute.working_dir(), Some("/abs"));

        let task = process_executor(
            &relative,
            config_dir,
            Some("task"),
            &env,
            ProcessRef::Task(0),
        );
        assert_eq!(task.working_dir(), Some("/cfg/task"));

        let inherited = process_executor(&relative, config_dir, None, &env, ProcessRef::Task(0));
        assert_eq!(inherited.working_dir(), Some("/cfg/sub"));
    }
}
