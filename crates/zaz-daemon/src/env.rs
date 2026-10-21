//! Process environment construction.
//!
//! Every process's complete environment is built here from a starting environment and the raw,
//! ordered layers of its chain. Group build and every task spawn call the same functions, so the
//! layering cannot drift between spawn sites.
//!
//! A process's environment is never derived from a parent executor's result. A level that selects
//! a different starting environment needs the whole chain rebuilt from the raw layers.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
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

    fn apply(&self, env: &mut EnvMap) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => env.insert(OsString::from(key), OsString::from(value)),
                None => env.remove(OsStr::new(key)),
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

/// A complete process environment.
///
/// Names and values are `OsString` because the daemon's own environment need not be UTF-8. Config
/// and `env_file` values are always UTF-8 and convert in.
pub(crate) type EnvMap = HashMap<OsString, OsString>;

/// The daemon's own environment.
pub(crate) fn inherited_env() -> EnvMap {
    std::env::vars_os().collect()
}

/// Build a complete environment from a starting environment and ordered layers.
///
/// Layers apply in order, so a later layer wins, and an unset removes the key at that point.
pub(crate) fn build_env<'a>(
    start: EnvMap,
    layers: impl IntoIterator<Item = &'a EnvLayer>,
) -> EnvMap {
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

    fn start(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn get<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
        env.get(OsStr::new(key)).and_then(|v| v.to_str())
    }

    fn file(pairs: &[(&str, &str)]) -> EnvLayer {
        EnvLayer::from_vars(&map(pairs))
    }

    fn unset(keys: &[&str]) -> EnvLayer {
        EnvLayer(keys.iter().map(|k| (k.to_string(), None)).collect())
    }

    #[test]
    fn later_layers_win() {
        let base = start(&[("A", "start"), ("B", "start"), ("C", "start")]);
        let group = EnvLayer::from_vars(&map(&[("B", "group"), ("C", "group")]));
        let process = EnvLayer::from_vars(&map(&[("C", "process")]));

        let env = build_env(base, &[group, process]);

        assert_eq!(get(&env, "A"), Some("start"));
        assert_eq!(get(&env, "B"), Some("group"));
        assert_eq!(get(&env, "C"), Some("process"));
    }

    #[test]
    fn unset_removes_the_key_and_a_later_set_restores_it() {
        let base = start(&[("A", "start"), ("B", "start")]);
        let reset = EnvLayer::from_vars(&map(&[("B", "again")]));

        let env = build_env(base, &[unset(&["A", "B"]), reset]);

        assert_eq!(get(&env, "A"), None);
        assert_eq!(get(&env, "B"), Some("again"));
    }

    #[test]
    fn a_non_utf8_start_entry_survives_the_merge_unchanged() {
        use std::os::unix::ffi::OsStringExt;

        let mut base = start(&[("A", "start")]);
        base.insert(
            OsString::from("RAW"),
            OsString::from_vec(vec![0xff, 0xfe, b'x']),
        );

        let env = build_env(base, &[file(&[("A", "layer")])]);

        assert_eq!(get(&env, "A"), Some("layer"));
        assert_eq!(
            env.get(OsStr::new("RAW")).unwrap(),
            &OsString::from_vec(vec![0xff, 0xfe, b'x'])
        );
    }

    #[test]
    fn a_non_utf8_inherited_value_does_not_panic_the_group_build() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(vec![b'v', 0xff]);
        std::env::set_var("ZAZ_TEST_NON_UTF8_INHERITED", &raw);

        let env = GroupEnv::default();
        let group = group_executor(None, Path::new("/cfg"), None, &env);
        let task = process_executor(&group, Path::new("/cfg"), None, &env, ProcessRef::Task(0));

        assert_eq!(
            task.env().get(OsStr::new("ZAZ_TEST_NON_UTF8_INHERITED")),
            Some(&raw)
        );

        std::env::remove_var("ZAZ_TEST_NON_UTF8_INHERITED");
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

        assert_eq!(
            get(task.env(), "ZAZ_TEST_MERGE_INHERITED"),
            Some("inherited")
        );
        assert_eq!(get(task.env(), "ZAZ_TEST_MERGE_SHADOWED"), Some("group"));
        assert_eq!(get(task.env(), "ZAZ_TEST_MERGE_OVERRIDDEN"), Some("task"));
        assert_eq!(get(group.env(), "ZAZ_TEST_MERGE_OVERRIDDEN"), Some("group"));
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

        assert_eq!(get(group.env(), "A"), Some("settings-file"));
        assert_eq!(get(group.env(), "B"), Some("group-file"));
        assert_eq!(get(group.env(), "C"), Some("group-inline"));
        assert_eq!(get(group.env(), "D"), Some("group-file"));
        assert_eq!(get(group.env(), "E"), None);

        assert_eq!(get(task.env(), "C"), Some("group-inline"));
        assert_eq!(get(task.env(), "D"), Some("task-file"));
        assert_eq!(get(task.env(), "E"), Some("task-inline"));
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

        assert_eq!(get(task1.env(), "T"), Some("1"));
        assert_eq!(get(task1.env(), "S"), None);
        assert_eq!(get(service0.env(), "S"), Some("0"));
        assert_eq!(get(service0.env(), "T"), None);
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
