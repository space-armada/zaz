//! The `env_command` fields a config declares.
//!
//! The daemon's evaluation step runs these commands. Validation and `zaz check` read the same
//! list, so every level is walked in one order and named one way.

use crate::env_file::EnvFileLevel;
use crate::resolve_against_config_dir;
use crate::schema::Config;
use std::path::{Path, PathBuf};

/// One declared `env_command`, with the directory it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvCommandSource {
    /// The level that declared the command.
    pub level: EnvFileLevel,
    /// Human-readable name of the field, such as `group 'api' env_command`.
    pub field: String,
    /// The command as written, before `${...}` expansion.
    pub command: String,
    /// The level's effective working directory.
    pub working_dir: PathBuf,
}

/// A declaration before its directory is resolved.
pub(crate) struct DeclaredEnvCommand<'a> {
    pub(crate) level: EnvFileLevel,
    pub(crate) field: String,
    pub(crate) command: &'a str,
    pub(crate) working_dir: Option<&'a str>,
    pub(crate) env_watch: &'a [String],
}

/// Every `env_command` a config declares, in settings, group, task, then service order.
///
/// A level's directory is its own `working_dir`, else its group's, else the config directory. A
/// relative path resolves against `config_dir`, which must be absolute.
pub fn env_command_sources(config: &Config, config_dir: &Path) -> Vec<EnvCommandSource> {
    declared_env_commands(config)
        .into_iter()
        .map(|declared| EnvCommandSource {
            level: declared.level,
            field: declared.field,
            command: declared.command.to_string(),
            working_dir: match declared.working_dir {
                Some(dir) => resolve_against_config_dir(config_dir, dir),
                None => config_dir.to_path_buf(),
            },
        })
        .collect()
}

/// One path a level lists in `env_watch`, resolved against the command's working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvWatchSource {
    /// The level that listed the path.
    pub level: EnvFileLevel,
    /// Human-readable name of the field, such as `group 'api' env_watch`.
    pub field: String,
    /// The path as written in the config.
    pub configured: String,
    /// The path joined onto the level's effective working directory. It is not normalized.
    pub path: PathBuf,
}

/// Every `env_watch` path a config lists, in the order of [`env_command_sources`].
///
/// A level's directory is the one its `env_command` runs in, so `config_dir` must be absolute.
pub fn env_watch_sources(config: &Config, config_dir: &Path) -> Vec<EnvWatchSource> {
    let mut sources = Vec::new();

    for declared in declared_env_commands(config) {
        let dir = match declared.working_dir {
            Some(dir) => resolve_against_config_dir(config_dir, dir),
            None => config_dir.to_path_buf(),
        };
        let field = declared.field.replace("env_command", "env_watch");

        for configured in declared.env_watch {
            sources.push(EnvWatchSource {
                level: declared.level,
                field: field.clone(),
                configured: configured.clone(),
                path: resolve_against_config_dir(&dir, configured),
            });
        }
    }

    sources
}

pub(crate) fn declared_env_commands(config: &Config) -> Vec<DeclaredEnvCommand<'_>> {
    let mut declared = Vec::new();

    if let Some(command) = &config.settings.env_command {
        declared.push(DeclaredEnvCommand {
            level: EnvFileLevel::Settings,
            field: "settings env_command".to_string(),
            command,
            working_dir: None,
            env_watch: &config.settings.env_watch,
        });
    }

    for (g, group) in config.groups.iter().enumerate() {
        let group_dir = group.working_dir.as_deref();

        if let Some(command) = &group.env_command {
            declared.push(DeclaredEnvCommand {
                level: EnvFileLevel::Group { group: g },
                field: format!("group '{}' env_command", group.name),
                command,
                working_dir: group_dir,
                env_watch: &group.env_watch,
            });
        }

        for (t, task) in group.tasks.iter().enumerate() {
            if let Some(command) = &task.env_command {
                declared.push(DeclaredEnvCommand {
                    level: EnvFileLevel::Task { group: g, task: t },
                    field: format!("group '{}' task '{}' env_command", group.name, task.name()),
                    command,
                    working_dir: task.working_dir.as_deref().or(group_dir),
                    env_watch: &task.env_watch,
                });
            }
        }

        for (s, service) in group.services.iter().enumerate() {
            if let Some(command) = &service.env_command {
                declared.push(DeclaredEnvCommand {
                    level: EnvFileLevel::Service {
                        group: g,
                        service: s,
                    },
                    field: format!(
                        "group '{}' service '{}' env_command",
                        group.name,
                        service.name()
                    ),
                    command,
                    working_dir: service.working_dir.as_deref().or(group_dir),
                    env_watch: &service.env_watch,
                });
            }
        }
    }

    declared
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_paths_resolve_against_the_command_directory() {
        let config = crate::parse_toml(
            "[settings]\nenv_command = \"s\"\nenv_watch = [\".envrc\"]\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\nworking_dir = \"ga\"\n\
             env_command = \"g\"\nenv_watch = [\"../flake.nix\", \"/abs/lock\"]\n",
        )
        .unwrap();

        let paths: Vec<_> = env_watch_sources(&config, Path::new("/cfg"))
            .into_iter()
            .map(|s| s.path)
            .collect();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/cfg/.envrc"),
                PathBuf::from("/cfg/ga/../flake.nix"),
                PathBuf::from("/abs/lock"),
            ]
        );
    }

    #[test]
    fn sources_resolve_the_effective_directory_of_each_level() {
        let config = crate::parse_toml(
            "[settings]\nenv_command = \"s\"\n\n\
             [[group]]\nname = \"a\"\npatterns = [\"*\"]\nworking_dir = \"ga\"\nenv_command = \"g\"\n\n\
             [[group.task]]\ncommand = \"t\"\nenv_command = \"t\"\n\n\
             [[group.task]]\nname = \"own\"\ncommand = \"t2\"\nworking_dir = \"/abs\"\nenv_command = \"t2\"\n\n\
             [[group.service]]\ncommand = \"sv\"\nenv_command = \"sv\"\nworking_dir = \"svc\"\n",
        )
        .unwrap();

        let sources = env_command_sources(&config, Path::new("/cfg"));
        let dirs: Vec<_> = sources.iter().map(|s| s.working_dir.clone()).collect();

        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/cfg"),
                PathBuf::from("/cfg/ga"),
                PathBuf::from("/cfg/ga"),
                PathBuf::from("/abs"),
                PathBuf::from("/cfg/svc"),
            ]
        );
        assert_eq!(sources[0].field, "settings env_command");
        assert_eq!(sources[1].field, "group 'a' env_command");
    }
}
