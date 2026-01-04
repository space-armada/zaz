//! Configuration validation.

use crate::error::{ValidationError, ValidationErrorKind, ValidationErrors};
use crate::Config;
use std::collections::{HashMap, HashSet};
use strsim::levenshtein;

/// Maximum Levenshtein distance to consider a suggestion.
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// Suggest a similar name from a list of valid options.
fn suggest_similar<'a>(unknown: &str, valid: &[&'a str]) -> Option<&'a str> {
    valid
        .iter()
        .filter(|&&v| {
            let dist = levenshtein(unknown, v);
            dist <= MAX_SUGGESTION_DISTANCE && dist > 0
        })
        .min_by_key(|&&v| levenshtein(unknown, v))
        .copied()
}

/// Format available options as a hint.
fn format_available_hint(available: &[&str]) -> String {
    // Filter out empty names
    let available: Vec<&str> = available
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect();
    if available.is_empty() {
        "no groups are defined".to_string()
    } else if available.len() <= 5 {
        format!("available groups are: {}", available.join(", "))
    } else {
        format!(
            "available groups are: {}, and {} more",
            available[..4].join(", "),
            available.len() - 4
        )
    }
}

/// Validate a configuration and return detailed errors.
pub fn validate(config: &Config) -> Result<(), ValidationErrors> {
    let mut errors = ValidationErrors::new();

    validate_groups(config, &mut errors);
    validate_dependencies(config, &mut errors);
    validate_patterns(config, &mut errors);
    validate_commands(config, &mut errors);

    errors.into_result()
}

/// Validate group definitions.
fn validate_groups(config: &Config, errors: &mut ValidationErrors) {
    let mut seen_names: HashMap<&str, usize> = HashMap::new();

    for (index, group) in config.groups.iter().enumerate() {
        // Check for empty group names
        if group.name.is_empty() {
            errors.push(ValidationError::new(ValidationErrorKind::EmptyGroupName {
                index,
            }));
        }

        // Check for duplicate group names
        if let Some(&first_index) = seen_names.get(group.name.as_str()) {
            errors.push(ValidationError::new(
                ValidationErrorKind::DuplicateGroupName {
                    name: group.name.clone(),
                    first_index,
                    second_index: index,
                },
            ));
        } else {
            seen_names.insert(&group.name, index);
        }

        // Check for empty patterns (warning-worthy but not an error)
        if group.patterns.is_empty() && group.tasks.is_empty() && group.daemons.is_empty() {
            errors.push(ValidationError::new(ValidationErrorKind::EmptyGroup {
                name: group.name.clone(),
            }));
        }
    }
}

/// Validate dependency references and detect cycles.
fn validate_dependencies(config: &Config, errors: &mut ValidationErrors) {
    let group_names: HashSet<&str> = config.groups.iter().map(|g| g.name.as_str()).collect();
    let group_names_vec: Vec<&str> = group_names.iter().copied().collect();

    // Check that all depends_on references exist
    for group in &config.groups {
        for dep in &group.depends_on {
            if !group_names.contains(dep.as_str()) {
                let mut error = ValidationError::new(ValidationErrorKind::UnknownDependency {
                    group: group.name.clone(),
                    dependency: dep.clone(),
                });

                // Add hint with suggestion or available groups
                if let Some(suggestion) = suggest_similar(dep, &group_names_vec) {
                    error = error.with_hint(format!("did you mean '{}'?", suggestion));
                } else {
                    error = error.with_hint(format_available_hint(&group_names_vec));
                }

                errors.push(error);
            }

            // Check for self-dependency
            if dep == &group.name {
                errors.push(ValidationError::new(ValidationErrorKind::SelfDependency {
                    group: group.name.clone(),
                }));
            }
        }
    }

    // Check for dependency cycles
    if let Some(cycle) = detect_cycle(config) {
        let cycle_str = cycle.join(" -> ");
        errors.push(
            ValidationError::new(ValidationErrorKind::DependencyCycle { cycle })
                .with_hint(format!("cycle: {}", cycle_str)),
        );
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
fn validate_patterns(config: &Config, errors: &mut ValidationErrors) {
    for group in &config.groups {
        for pattern in &group.patterns {
            if let Err(e) = globset::Glob::new(pattern) {
                errors.push(ValidationError::new(ValidationErrorKind::InvalidPattern {
                    group: group.name.clone(),
                    pattern: pattern.clone(),
                    error: e.to_string(),
                }));
            }
        }

        for pattern in &group.ignore {
            if let Err(e) = globset::Glob::new(pattern) {
                errors.push(ValidationError::new(
                    ValidationErrorKind::InvalidIgnorePattern {
                        group: group.name.clone(),
                        pattern: pattern.clone(),
                        error: e.to_string(),
                    },
                ));
            }
        }
    }
}

/// Validate command definitions.
fn validate_commands(config: &Config, errors: &mut ValidationErrors) {
    for group in &config.groups {
        // Check task commands
        let mut task_names: HashSet<&str> = HashSet::new();
        for task in &group.tasks {
            let name = task.name();
            if task.command.is_empty() {
                errors.push(ValidationError::new(
                    ValidationErrorKind::EmptyTaskCommand {
                        group: group.name.clone(),
                        task: name.to_string(),
                    },
                ));
            }
            if task_names.contains(name) {
                errors.push(ValidationError::new(
                    ValidationErrorKind::DuplicateTaskName {
                        group: group.name.clone(),
                        name: name.to_string(),
                        is_explicit: task.has_explicit_name(),
                    },
                ));
            }
            task_names.insert(name);
        }

        // Check daemon commands
        let mut daemon_names: HashSet<&str> = HashSet::new();
        for daemon in &group.daemons {
            let name = daemon.name();
            if daemon.command.is_empty() {
                errors.push(ValidationError::new(
                    ValidationErrorKind::EmptyDaemonCommand {
                        group: group.name.clone(),
                        daemon: name.to_string(),
                    },
                ));
            }
            if daemon_names.contains(name) {
                errors.push(ValidationError::new(
                    ValidationErrorKind::DuplicateDaemonName {
                        group: group.name.clone(),
                        name: name.to_string(),
                        is_explicit: daemon.has_explicit_name(),
                    },
                ));
            }
            daemon_names.insert(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Group, TaskCommand};

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
    fn test_empty_task_command() {
        let mut group = make_group("backend");
        group.tasks = vec![TaskCommand::new("test", "")];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("empty command"));
    }

    #[test]
    fn test_duplicate_task_names_explicit() {
        // Explicit duplicate names should error without hint
        let mut group = make_group("backend");
        group.tasks = vec![
            TaskCommand::new("test", "echo 1"),
            TaskCommand::new("test", "echo 2"),
        ];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate task name 'test'"));
        // Explicit names should NOT get the hint
        assert!(!msg.contains("use explicit 'name' field"));
    }

    #[test]
    fn test_duplicate_task_names_derived() {
        // Derived duplicate names should error with hint
        // Both commands derive to "cargo" (flags stop the derivation)
        let mut group = make_group("backend");
        group.tasks = vec![
            TaskCommand::from_command("cargo --version"),
            TaskCommand::from_command("cargo -V"),
        ];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate task name 'cargo'"));
        assert!(msg.contains("use explicit 'name' field to disambiguate"));
    }

    #[test]
    fn test_explicit_names_disambiguate() {
        // Explicit names should allow same-prefix commands
        let mut group = make_group("backend");
        group.tasks = vec![
            TaskCommand::new("build", "cargo build"),
            TaskCommand::new("test", "cargo test"),
        ];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_derived_names_unique() {
        // Different derived names should be valid
        let mut group = make_group("backend");
        group.tasks = vec![
            TaskCommand::from_command("cargo build"),
            TaskCommand::from_command("npm test"),
        ];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_mixed_explicit_and_derived_names() {
        // Mix of explicit and derived names that don't conflict
        let mut group = make_group("backend");
        group.tasks = vec![
            TaskCommand::new("build", "cargo build --release"),
            TaskCommand::from_command("npm install"),
        ];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_derived_name_conflicts_with_explicit() {
        // Derived name that conflicts with an explicit name
        // "cargo --help" derives to "cargo" which conflicts with explicit "cargo"
        let mut group = make_group("backend");
        group.tasks = vec![
            TaskCommand::new("cargo", "echo explicit"),
            TaskCommand::from_command("cargo --help"),
        ];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate task name 'cargo'"));
        // The derived one should get the hint
        assert!(msg.contains("use explicit 'name' field to disambiguate"));
    }

    #[test]
    fn test_suggest_similar_basic() {
        // Test the suggest_similar helper function
        let valid = vec!["backend", "frontend", "protobuf"];
        assert_eq!(suggest_similar("bacend", &valid), Some("backend")); // 1 char diff
        assert_eq!(suggest_similar("frontent", &valid), Some("frontend")); // 1 char diff
        assert_eq!(suggest_similar("protobufs", &valid), Some("protobuf")); // 1 char diff
        assert_eq!(suggest_similar("totally_different", &valid), None); // too different
    }

    #[test]
    fn test_unknown_dependency_with_typo_hint() {
        // Typo in dependency name should suggest the correct name
        let mut frontend = make_group("frontend");
        frontend.depends_on = vec!["bacend".to_string()]; // typo: should be "backend"
        let config = Config {
            groups: vec![make_group("backend"), frontend],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown group 'bacend'"));
        assert!(msg.contains("did you mean 'backend'?"));
    }

    #[test]
    fn test_unknown_dependency_lists_available() {
        // No close match should list available groups
        let mut frontend = make_group("frontend");
        frontend.depends_on = vec!["totally_different".to_string()];
        let config = Config {
            groups: vec![make_group("backend"), make_group("protobuf"), frontend],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown group 'totally_different'"));
        assert!(msg.contains("available groups are:"));
        assert!(msg.contains("backend"));
        assert!(msg.contains("protobuf"));
    }

    #[test]
    fn test_dependency_cycle_hint() {
        // Cycle detection should include the cycle path in hint
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
        let msg = err.to_string();
        assert!(msg.contains("cycle detected"));
        assert!(msg.contains("hint: cycle:"));
    }

    #[test]
    fn test_format_available_hint() {
        assert_eq!(format_available_hint(&[]), "no groups are defined");
        assert_eq!(
            format_available_hint(&["a", "b"]),
            "available groups are: a, b"
        );
        assert_eq!(
            format_available_hint(&["a", "b", "c", "d", "e", "f"]),
            "available groups are: a, b, c, d, and 2 more"
        );
    }
}
