//! Configuration validation.

use crate::{Config, ConfigError};
use std::collections::{HashMap, HashSet};

/// Validate a configuration and return detailed errors.
pub fn validate(config: &Config) -> Result<(), ConfigError> {
    let mut errors = Vec::new();

    validate_groups(config, &mut errors);
    validate_dependencies(config, &mut errors);
    validate_patterns(config, &mut errors);
    validate_commands(config, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(errors.join("\n")))
    }
}

/// Validate group definitions.
fn validate_groups(config: &Config, errors: &mut Vec<String>) {
    let mut seen_names: HashMap<&str, usize> = HashMap::new();

    for (index, group) in config.groups.iter().enumerate() {
        // Check for empty group names
        if group.name.is_empty() {
            errors.push(format!("group[{}]: name cannot be empty", index));
        }

        // Check for duplicate group names
        if let Some(&first_index) = seen_names.get(group.name.as_str()) {
            errors.push(format!(
                "group[{}]: duplicate name '{}' (first defined at group[{}])",
                index, group.name, first_index
            ));
        } else {
            seen_names.insert(&group.name, index);
        }

        // Check for empty patterns (warning-worthy but not an error)
        if group.patterns.is_empty() && group.prep.is_empty() && group.daemons.is_empty() {
            errors.push(format!(
                "group '{}': has no patterns and no commands",
                group.name
            ));
        }
    }
}

/// Validate dependency references and detect cycles.
fn validate_dependencies(config: &Config, errors: &mut Vec<String>) {
    let group_names: HashSet<&str> = config.groups.iter().map(|g| g.name.as_str()).collect();

    // Check that all depends_on references exist
    for group in &config.groups {
        for dep in &group.depends_on {
            if !group_names.contains(dep.as_str()) {
                errors.push(format!(
                    "group '{}': depends_on references unknown group '{}'",
                    group.name, dep
                ));
            }

            // Check for self-dependency
            if dep == &group.name {
                errors.push(format!("group '{}': cannot depend on itself", group.name));
            }
        }
    }

    // Check for dependency cycles
    if let Some(cycle) = detect_cycle(config) {
        errors.push(format!("dependency cycle detected: {}", cycle.join(" -> ")));
    }
}

/// Detect dependency cycles using DFS.
fn detect_cycle(config: &Config) -> Option<Vec<String>> {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut path = Vec::new();

    // Build adjacency map
    let deps: HashMap<&str, Vec<&str>> = config
        .groups
        .iter()
        .map(|g| {
            (
                g.name.as_str(),
                g.depends_on.iter().map(|s| s.as_str()).collect(),
            )
        })
        .collect();

    for group in &config.groups {
        if !visited.contains(group.name.as_str()) {
            if let Some(cycle) =
                dfs_cycle(&group.name, &deps, &mut visiting, &mut visited, &mut path)
            {
                return Some(cycle);
            }
        }
    }

    None
}

fn dfs_cycle<'a>(
    node: &'a str,
    deps: &HashMap<&'a str, Vec<&'a str>>,
    visiting: &mut HashSet<&'a str>,
    visited: &mut HashSet<&'a str>,
    path: &mut Vec<String>,
) -> Option<Vec<String>> {
    if visiting.contains(node) {
        // Found a cycle - extract it from the path
        path.push(node.to_string());
        let cycle_start = path.iter().position(|n| n == node).unwrap();
        return Some(path[cycle_start..].to_vec());
    }

    if visited.contains(node) {
        return None;
    }

    visiting.insert(node);
    path.push(node.to_string());

    if let Some(neighbors) = deps.get(node) {
        for &neighbor in neighbors {
            if let Some(cycle) = dfs_cycle(neighbor, deps, visiting, visited, path) {
                return Some(cycle);
            }
        }
    }

    path.pop();
    visiting.remove(node);
    visited.insert(node);
    None
}

/// Validate glob patterns.
fn validate_patterns(config: &Config, errors: &mut Vec<String>) {
    for group in &config.groups {
        for pattern in &group.patterns {
            if let Err(e) = globset::Glob::new(pattern) {
                errors.push(format!(
                    "group '{}': invalid pattern '{}': {}",
                    group.name, pattern, e
                ));
            }
        }

        for pattern in &group.ignore {
            if let Err(e) = globset::Glob::new(pattern) {
                errors.push(format!(
                    "group '{}': invalid ignore pattern '{}': {}",
                    group.name, pattern, e
                ));
            }
        }
    }
}

/// Validate command definitions.
fn validate_commands(config: &Config, errors: &mut Vec<String>) {
    for group in &config.groups {
        // Check prep commands
        let mut prep_names: HashSet<&str> = HashSet::new();
        for (index, prep) in group.prep.iter().enumerate() {
            if prep.name.is_empty() {
                errors.push(format!(
                    "group '{}': prep[{}] has empty name",
                    group.name, index
                ));
            }
            if prep.command.is_empty() {
                errors.push(format!(
                    "group '{}': prep '{}' has empty command",
                    group.name, prep.name
                ));
            }
            if prep_names.contains(prep.name.as_str()) {
                errors.push(format!(
                    "group '{}': duplicate prep name '{}'",
                    group.name, prep.name
                ));
            }
            prep_names.insert(&prep.name);
        }

        // Check daemon commands
        let mut daemon_names: HashSet<&str> = HashSet::new();
        for (index, daemon) in group.daemons.iter().enumerate() {
            if daemon.name.is_empty() {
                errors.push(format!(
                    "group '{}': daemon[{}] has empty name",
                    group.name, index
                ));
            }
            if daemon.command.is_empty() {
                errors.push(format!(
                    "group '{}': daemon '{}' has empty command",
                    group.name, daemon.name
                ));
            }
            if daemon_names.contains(daemon.name.as_str()) {
                errors.push(format!(
                    "group '{}': duplicate daemon name '{}'",
                    group.name, daemon.name
                ));
            }
            daemon_names.insert(&daemon.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Group, PrepCommand};

    fn make_group(name: &str) -> Group {
        Group {
            name: name.to_string(),
            patterns: vec!["**/*.rs".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn test_valid_config() {
        let config = Config {
            groups: vec![make_group("backend"), make_group("frontend")],
            ..Default::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_duplicate_group_names() {
        let config = Config {
            groups: vec![make_group("backend"), make_group("backend")],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("duplicate name"));
    }

    #[test]
    fn test_empty_group_name() {
        let config = Config {
            groups: vec![make_group("")],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("name cannot be empty"));
    }

    #[test]
    fn test_unknown_dependency() {
        let mut group = make_group("frontend");
        group.depends_on = vec!["nonexistent".to_string()];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("unknown group"));
    }

    #[test]
    fn test_self_dependency() {
        let mut group = make_group("backend");
        group.depends_on = vec!["backend".to_string()];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("cannot depend on itself"));
    }

    #[test]
    fn test_dependency_cycle() {
        let mut a = make_group("a");
        a.depends_on = vec!["b".to_string()];
        let mut b = make_group("b");
        b.depends_on = vec!["c".to_string()];
        let mut c = make_group("c");
        c.depends_on = vec!["a".to_string()];

        let config = Config {
            groups: vec![a, b, c],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("cycle detected"));
    }

    #[test]
    fn test_invalid_pattern() {
        let mut group = make_group("backend");
        group.patterns = vec!["[invalid".to_string()];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("invalid pattern"));
    }

    #[test]
    fn test_empty_prep_command() {
        let mut group = make_group("backend");
        group.prep = vec![PrepCommand {
            name: "test".to_string(),
            command: "".to_string(),
            on_change_only: false,
        }];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("empty command"));
    }

    #[test]
    fn test_duplicate_prep_names() {
        let mut group = make_group("backend");
        group.prep = vec![
            PrepCommand {
                name: "test".to_string(),
                command: "echo 1".to_string(),
                on_change_only: false,
            },
            PrepCommand {
                name: "test".to_string(),
                command: "echo 2".to_string(),
                on_change_only: false,
            },
        ];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("duplicate prep name"));
    }
}
