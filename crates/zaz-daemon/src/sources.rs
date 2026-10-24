//! The files a config's environment is read from, and which levels each one feeds.
//!
//! The evaluation step hashes and matches these paths, and the engine registers them with the
//! watcher. Both need the same answer to "which paths belong to which level", so it lives here.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use zaz_config::{Config, EnvFileLevel};

/// The `env_watch` paths of every level that declares an `env_command`.
///
/// Levels whose commands are textually the same and run in the same directory share one
/// evaluation, so each of them gets the union of their lists. A change to any listed file then
/// refreshes every level that shares the command.
pub(crate) fn command_watch_paths(
    config: &Config,
    config_dir: &Path,
) -> HashMap<EnvFileLevel, Vec<PathBuf>> {
    let commands = zaz_config::env_command_sources(config, config_dir);
    let watches = zaz_config::env_watch_sources(config, config_dir);
    let mut by_level = HashMap::new();

    for command in &commands {
        let mut paths: Vec<PathBuf> = Vec::new();

        for watch in &watches {
            let shares = commands.iter().any(|other| {
                other.level == watch.level
                    && other.command == command.command
                    && other.working_dir == command.working_dir
            });

            if shares && !paths.contains(&watch.path) {
                paths.push(watch.path.clone());
            }
        }

        by_level.insert(command.level, paths);
    }

    by_level
}

/// Every watched source of a config, keyed by its path in the watcher's normal form.
#[derive(Debug, Clone, Default)]
pub(crate) struct SourceIndex {
    by_key: BTreeMap<PathBuf, Vec<EnvFileLevel>>,
    paths: Vec<PathBuf>,
    listed: Vec<ListedPath>,
}

/// One path an `env_watch` list names, kept for the load-time log.
#[derive(Debug, Clone)]
pub(crate) struct ListedPath {
    pub(crate) field: String,
    pub(crate) path: PathBuf,
}

impl SourceIndex {
    pub(crate) fn build(config: &Config, config_dir: &Path) -> Self {
        let mut index = Self::default();

        for source in zaz_config::env_file_sources(config, config_dir) {
            index.add(source.level, source.path);
        }

        let shared = command_watch_paths(config, config_dir);
        for (level, paths) in &shared {
            for path in paths {
                index.add(*level, path.clone());
            }
        }

        for source in zaz_config::env_watch_sources(config, config_dir) {
            index.listed.push(ListedPath {
                field: source.field,
                path: source.path,
            });
        }

        index
    }

    fn add(&mut self, level: EnvFileLevel, path: PathBuf) {
        let key = zaz_watch::normalize(&path);
        let levels = self.by_key.entry(key).or_default();

        if !levels.contains(&level) {
            levels.push(level);
        }
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
    }

    /// The paths to register with the watcher, as configured.
    pub(crate) fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// The paths an `env_watch` list names, with the field that named each.
    pub(crate) fn listed(&self) -> &[ListedPath] {
        &self.listed
    }

    /// The levels that read the source with this key.
    pub(crate) fn levels(&self, key: &Path) -> &[EnvFileLevel] {
        self.by_key.get(key).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// The names of the groups whose environment the given levels feed.
///
/// A settings level feeds every group. Any other level feeds the group it sits in.
pub(crate) fn groups_fed_by(config: &Config, levels: &[EnvFileLevel]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();

    for level in levels {
        let index = match *level {
            EnvFileLevel::Settings => {
                names.extend(config.groups.iter().map(|g| g.name.clone()));
                continue;
            }
            EnvFileLevel::Group { group }
            | EnvFileLevel::Task { group, .. }
            | EnvFileLevel::Service { group, .. } => group,
        };

        if let Some(group) = config.groups.get(index) {
            names.push(group.name.clone());
        }
    }

    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        zaz_config::parse_toml(toml).unwrap()
    }

    #[test]
    fn levels_that_share_a_command_share_its_watch_paths() {
        let config = config(
            "[[group]]\nname = \"a\"\nenv_command = \"direnv export json\"\nenv_watch = [\".envrc\"]\n\n\
             [[group]]\nname = \"b\"\nenv_command = \"direnv export json\"\nenv_watch = [\"flake.nix\"]\n\n\
             [[group]]\nname = \"c\"\nenv_command = \"other\"\nenv_watch = [\"c.lock\"]\n",
        );
        let paths = command_watch_paths(&config, Path::new("/cfg"));

        let a = &paths[&EnvFileLevel::Group { group: 0 }];
        assert_eq!(
            a,
            &[
                PathBuf::from("/cfg/.envrc"),
                PathBuf::from("/cfg/flake.nix")
            ]
        );
        assert_eq!(paths[&EnvFileLevel::Group { group: 1 }], *a);
        assert_eq!(
            paths[&EnvFileLevel::Group { group: 2 }],
            [PathBuf::from("/cfg/c.lock")]
        );
    }

    #[test]
    fn a_settings_source_feeds_every_group_and_a_group_source_only_its_own() {
        let config = config(
            "[[group]]\nname = \"a\"\n\n[[group.task]]\ncommand = \"t\"\n\n[[group]]\nname = \"b\"\n",
        );

        assert_eq!(
            groups_fed_by(&config, &[EnvFileLevel::Settings]),
            ["a", "b"]
        );
        assert_eq!(
            groups_fed_by(&config, &[EnvFileLevel::Task { group: 0, task: 0 }]),
            ["a"]
        );
    }
}
