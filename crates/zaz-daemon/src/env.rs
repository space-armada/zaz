//! Process environment construction.
//!
//! Every process's complete environment is built here from a starting environment and the raw,
//! ordered layers of its chain. Group build and every task spawn call the same functions, so the
//! layering cannot drift between spawn sites.
//!
//! A process's environment is never derived from a parent executor's result. A level that selects
//! a different starting environment needs the whole chain rebuilt from the raw layers.
//!
//! A chain starts from the daemon's environment, or from the base environment when any level of
//! it sets an `env_command`. The base keeps a fixed list of platform and session variables, so
//! the output of a command like `direnv export json` means the same whatever shell started the
//! daemon.

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

    /// A layer that holds the sets and unsets an `env_command` printed.
    pub(crate) fn from_output(output: BTreeMap<String, Option<String>>) -> Self {
        Self(output)
    }

    /// The names this layer sets, in order.
    pub(crate) fn set_keys(&self) -> Vec<String> {
        self.keys_where(|value| value.is_some())
    }

    /// The names this layer unsets, in order.
    pub(crate) fn unset_keys(&self) -> Vec<String> {
        self.keys_where(|value| value.is_none())
    }

    fn keys_where(&self, keep: impl Fn(&Option<String>) -> bool) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, value)| keep(value))
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(crate) fn apply(&self, env: &mut EnvMap) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => env.insert(OsString::from(key), OsString::from(value)),
                None => env.remove(OsStr::new(key)),
            };
        }
    }
}

/// The environment a chain of layers is applied to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Start {
    /// The daemon's own environment.
    #[default]
    Inherited,
    /// The base environment plus the named extra variables.
    Base(Passthrough),
}

impl Start {
    /// The starting environment, read from the daemon's environment now.
    pub(crate) fn build(&self) -> EnvMap {
        match self {
            Self::Inherited => inherited_env(),
            Self::Base(passthrough) => base_env(passthrough),
        }
    }
}

/// The ordered layers one task or service contributes, and the environment they apply to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProcessEnv {
    start: Start,
    layers: Vec<EnvLayer>,
}

/// The ordered layers a group contributes, with each task's and service's own layers.
///
/// This is the evaluated environment of one group. It holds loaded `env_file` values and
/// `env_command` output, so an edited source makes the value compare unequal and the reload diff
/// rebuilds the group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GroupEnv {
    group_start: Start,
    group: Vec<EnvLayer>,
    tasks: Vec<ProcessEnv>,
    services: Vec<ProcessEnv>,
}

impl GroupEnv {
    /// Layers that all apply to the daemon's own environment.
    #[cfg(test)]
    pub(crate) fn new(
        group: Vec<EnvLayer>,
        tasks: Vec<Vec<EnvLayer>>,
        services: Vec<Vec<EnvLayer>>,
    ) -> Self {
        let inherited = |layers| ProcessEnv {
            start: Start::Inherited,
            layers,
        };

        Self {
            group_start: Start::Inherited,
            group,
            tasks: tasks.into_iter().map(inherited).collect(),
            services: services.into_iter().map(inherited).collect(),
        }
    }

    /// Layers with the starting environment of the group and of each task and service.
    ///
    /// `tasks` and `services` pair each process's own layers with its start.
    pub(crate) fn with_starts(
        group_start: Start,
        group: Vec<EnvLayer>,
        tasks: Vec<(Start, Vec<EnvLayer>)>,
        services: Vec<(Start, Vec<EnvLayer>)>,
    ) -> Self {
        let process = |(start, layers)| ProcessEnv { start, layers };

        Self {
            group_start,
            group,
            tasks: tasks.into_iter().map(process).collect(),
            services: services.into_iter().map(process).collect(),
        }
    }

    fn own(&self, process: ProcessRef) -> Option<&ProcessEnv> {
        match process {
            ProcessRef::Task(index) => self.tasks.get(index),
            ProcessRef::Service(index) => self.services.get(index),
        }
    }

    fn process_start(&self, process: ProcessRef) -> Start {
        self.own(process)
            .map(|own| own.start.clone())
            .unwrap_or_else(|| self.group_start.clone())
    }

    fn process(&self, process: ProcessRef) -> impl Iterator<Item = &EnvLayer> {
        self.group
            .iter()
            .chain(self.own(process).into_iter().flat_map(|own| &own.layers))
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

/// Names the base environment keeps from the daemon's environment.
///
/// The proxy and CA variables let a cold evaluation that downloads work behind a proxy.
const BASE_NAMES: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "TMPDIR",
    "TZ",
    "SSH_AUTH_SOCK",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "CI",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Prefixes whose every variable the base keeps.
///
/// `XDG_*` is required. direnv finds its allow list under `XDG_DATA_HOME`.
const BASE_PREFIXES: &[&str] = &["LC_", "XDG_"];

/// The extra names an `env_passthrough` list adds to the base.
///
/// An entry is a name, or a prefix followed by one trailing `*`. Config validation guarantees
/// that shape. The list only adds to the fixed base.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Passthrough(Vec<String>);

impl Passthrough {
    pub(crate) fn new(entries: &[String]) -> Self {
        Self(entries.to_vec())
    }

    fn keeps(&self, name: &str) -> bool {
        self.0.iter().any(|entry| match entry.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => entry == name,
        })
    }
}

fn base_keeps(name: &OsStr, passthrough: &Passthrough) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };

    BASE_NAMES.contains(&name)
        || BASE_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || passthrough.keeps(name)
}

/// The base environment: the kept variables of the daemon's environment.
pub(crate) fn base_env(passthrough: &Passthrough) -> EnvMap {
    std::env::vars_os()
        .filter(|(name, _)| base_keeps(name, passthrough))
        .collect()
}

/// The names of the daemon's variables that the base leaves out, sorted.
pub(crate) fn dropped_names(passthrough: &Passthrough) -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter(|(name, _)| !base_keeps(name, passthrough))
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    names.sort();

    names
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
        .with_env(build_env(env.group_start.build(), &env.group));

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
    let executor = group.clone().with_env(build_env(
        env.process_start(process).build(),
        env.process(process),
    ));

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

    /// Removes a process variable when dropped, so a failed assertion cannot leak it into other
    /// tests.
    struct InheritedVarGuard(&'static str);

    impl InheritedVarGuard {
        fn set(key: &'static str, value: &OsStr) -> Self {
            std::env::set_var(key, value);
            Self(key)
        }
    }

    impl Drop for InheritedVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    #[test]
    fn a_non_utf8_inherited_value_does_not_panic_the_group_build() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(vec![b'v', 0xff]);
        let _guard = InheritedVarGuard::set("ZAZ_TEST_NON_UTF8_INHERITED", &raw);

        let env = GroupEnv::default();
        let group = group_executor(None, Path::new("/cfg"), None, &env);
        let task = process_executor(&group, Path::new("/cfg"), None, &env, ProcessRef::Task(0));

        assert_eq!(
            task.env().get(OsStr::new("ZAZ_TEST_NON_UTF8_INHERITED")),
            Some(&raw)
        );
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

    #[test]
    fn the_base_keeps_the_fixed_names_and_prefixes_and_the_passthrough_additions() {
        let none = Passthrough::default();
        let extra = Passthrough::new(&["MY_TOOL_HOME".to_string(), "OP_*".to_string()]);

        for kept in [
            "HOME",
            "PATH",
            "USER",
            "LANG",
            "LC_ALL",
            "XDG_DATA_HOME",
            "TZ",
            "http_proxy",
            "SSL_CERT_DIR",
            "SSH_AUTH_SOCK",
            "CI",
        ] {
            assert!(base_keeps(OsStr::new(kept), &none), "{kept}");
        }

        for dropped in [
            "AWS_PROFILE",
            "KUBECONFIG",
            "OP_SESSION_x",
            "MY_TOOL_HOME",
            "LCX",
            "NO_PROXYX",
        ] {
            assert!(!base_keeps(OsStr::new(dropped), &none), "{dropped}");
        }

        assert!(base_keeps(OsStr::new("MY_TOOL_HOME"), &extra));
        assert!(base_keeps(OsStr::new("OP_SESSION_x"), &extra));
        assert!(base_keeps(OsStr::new("PATH"), &extra));
        assert!(!base_keeps(OsStr::new("MY_TOOL_HOME2"), &extra));
    }

    #[test]
    fn a_non_utf8_name_is_dropped_from_the_base() {
        use std::os::unix::ffi::OsStrExt;

        assert!(!base_keeps(
            OsStr::from_bytes(&[b'H', 0xff]),
            &Passthrough::default()
        ));
    }
}
