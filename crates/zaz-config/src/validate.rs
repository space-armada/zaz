//! Configuration validation.

use crate::error::{ValidationError, ValidationErrorKind, ValidationErrors};
use crate::toml_spanned::SpanInfo;
use crate::{Config, ServiceCommand};
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
    validate_with_spans(config, "", None)
}

/// Validate a configuration with optional span information for line numbers.
pub fn validate_with_spans(
    config: &Config,
    source: &str,
    span_info: Option<&SpanInfo>,
) -> Result<(), ValidationErrors> {
    let mut errors = ValidationErrors::new();

    validate_groups(config, source, span_info, &mut errors);
    validate_dependencies(config, source, span_info, &mut errors);
    validate_patterns(config, &mut errors);
    validate_commands(config, &mut errors);

    errors.into_result()
}

/// Validate group definitions.
fn validate_groups(
    config: &Config,
    source: &str,
    span_info: Option<&SpanInfo>,
    errors: &mut ValidationErrors,
) {
    let mut seen_names: HashMap<&str, usize> = HashMap::new();

    for (index, group) in config.groups.iter().enumerate() {
        // Get span for this group's name if available
        let name_span = span_info.and_then(|si| si.group_name_span(source, index));

        // Check for empty group names
        if group.name.is_empty() {
            let mut error = ValidationError::new(ValidationErrorKind::EmptyGroupName { index });
            if let Some(span) = name_span {
                error = error.with_span(span);
            }
            errors.push(error);
        }

        // Check for duplicate group names
        if let Some(&first_index) = seen_names.get(group.name.as_str()) {
            let mut error = ValidationError::new(ValidationErrorKind::DuplicateGroupName {
                name: group.name.clone(),
                first_index,
                second_index: index,
            });
            if let Some(span) = name_span {
                error = error.with_span(span);
            }
            errors.push(error);
        } else {
            seen_names.insert(&group.name, index);
        }

        // Check for empty patterns (warning-worthy but not an error)
        if group.patterns.is_empty() && group.tasks.is_empty() && group.services.is_empty() {
            let mut error = ValidationError::new(ValidationErrorKind::EmptyGroup {
                name: group.name.clone(),
            });
            if let Some(span) = name_span {
                error = error.with_span(span);
            }
            errors.push(error);
        }
    }
}

/// Validate dependency references and detect cycles.
fn validate_dependencies(
    config: &Config,
    source: &str,
    span_info: Option<&SpanInfo>,
    errors: &mut ValidationErrors,
) {
    let group_names: HashSet<&str> = config.groups.iter().map(|g| g.name.as_str()).collect();
    let group_names_vec: Vec<&str> = group_names.iter().copied().collect();

    // Check that all depends_on references exist
    for (group_idx, group) in config.groups.iter().enumerate() {
        for (dep_idx, dep) in group.depends_on.iter().enumerate() {
            // Get span for this dependency if available
            let dep_span = span_info.and_then(|si| si.dependency_span(source, group_idx, dep_idx));

            if !group_names.contains(dep.as_str()) {
                let mut error = ValidationError::new(ValidationErrorKind::UnknownDependency {
                    group: group.name.clone(),
                    dependency: dep.clone(),
                });

                // Add span if available
                if let Some(span) = dep_span {
                    error = error.with_span(span);
                }

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
                let mut error = ValidationError::new(ValidationErrorKind::SelfDependency {
                    group: group.name.clone(),
                });
                if let Some(span) = dep_span {
                    error = error.with_span(span);
                }
                errors.push(error);
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
                let mut error = ValidationError::new(ValidationErrorKind::DuplicateTaskName {
                    group: group.name.clone(),
                    name: name.to_string(),
                });
                if !task.has_explicit_name() {
                    error = error.with_hint("use explicit 'name' field to disambiguate");
                }
                errors.push(error);
            }
            task_names.insert(name);
        }

        // Check service commands
        let mut service_names: HashSet<&str> = HashSet::new();
        for service in &group.services {
            let name = service.name();

            validate_service_command_field(&group.name, name, "command", &service.command, errors);
            if let Some(cleanup) = &service.cleanup_command {
                validate_service_command_field(
                    &group.name,
                    name,
                    "cleanup_command",
                    cleanup,
                    errors,
                );
            }
            if let Some(ready) = &service.ready_check {
                validate_service_command_field(&group.name, name, "ready_check", ready, errors);
            }
            if let Some(stop) = &service.stop_command {
                validate_service_command_field(&group.name, name, "stop_command", stop, errors);
            }
            if let Some(kill) = &service.kill_command {
                validate_service_command_field(&group.name, name, "kill_command", kill, errors);
            }

            validate_service_stop_mechanism(&group.name, service, errors);
            validate_service_readiness(&group.name, service, errors);

            if service_names.contains(name) {
                let mut error = ValidationError::new(ValidationErrorKind::DuplicateServiceName {
                    group: group.name.clone(),
                    name: name.to_string(),
                });
                if !service.has_explicit_name() {
                    error = error.with_hint("use explicit 'name' field to disambiguate");
                }
                errors.push(error);
            }
            service_names.insert(name);
        }
    }
}

/// Validate one command-carrying field of a service.
///
/// Every service field that becomes a shell command shares these rules, so `command` and
/// the lifecycle hooks run the same checks rather than each growing its own copy.
fn validate_service_command_field(
    group: &str,
    service: &str,
    field: &str,
    command: &str,
    errors: &mut ValidationErrors,
) {
    if command.is_empty() {
        errors.push(ValidationError::new(
            ValidationErrorKind::EmptyServiceCommand {
                group: group.to_string(),
                service: service.to_string(),
                field: field.to_string(),
            },
        ));
    }

    // Services run wholesale, not per-file. References to file-context
    // built-ins would silently expand to empty strings at spawn time.
    for var_name in zaz_vars::references(command) {
        if zaz_vars::FILE_CONTEXT_BUILTINS.contains(&var_name) {
            errors.push(
                ValidationError::new(ValidationErrorKind::ServiceCommandFileBuiltin {
                    group: group.to_string(),
                    service: service.to_string(),
                    field: field.to_string(),
                    builtin: var_name.to_string(),
                })
                .with_hint("move this expansion into a [[group.task]] that runs on file changes"),
            );
        }
    }
}

/// Reject a service whose stop mechanism is specified twice.
///
/// `stop_command` replaces the signal outright, so a `signal` set alongside it is silently
/// ignored at runtime. Rejecting the pair at load time keeps that from reading as working.
fn validate_service_stop_mechanism(
    group: &str,
    service: &ServiceCommand,
    errors: &mut ValidationErrors,
) {
    if service.has_explicit_signal() && service.stop_command.is_some() {
        errors.push(
            ValidationError::new(ValidationErrorKind::ConflictingStopMechanism {
                group: group.to_string(),
                service: service.name().to_string(),
            })
            .with_hint("stop_command replaces the restart signal; remove one"),
        );
    }
}

/// Validate a service's readiness configuration.
///
/// `ready_poll_interval` and `ready_timeout` only ever apply to a `ready_check`, so either one
/// set alone does nothing at runtime. Rejecting the pairing keeps a config that gates nothing
/// from reading as if it does.
///
/// A zero poll interval is rejected separately: it would run the check back to back with no
/// pause, spinning until the timeout rather than polling.
fn validate_service_readiness(
    group: &str,
    service: &ServiceCommand,
    errors: &mut ValidationErrors,
) {
    if service.ready_check.is_none() {
        for (field, set) in [
            ("ready_poll_interval", service.ready_poll_interval.is_some()),
            ("ready_timeout", service.ready_timeout.is_some()),
        ] {
            if !set {
                continue;
            }

            errors.push(
                ValidationError::new(ValidationErrorKind::ReadyTuningWithoutCheck {
                    group: group.to_string(),
                    service: service.name().to_string(),
                    field: field.to_string(),
                })
                .with_hint("this only applies to a ready_check; add one or remove the field"),
            );
        }
    }

    if service.ready_poll_interval_ms() == Some(0) {
        errors.push(
            ValidationError::new(ValidationErrorKind::ZeroReadyPollInterval {
                group: group.to_string(),
                service: service.name().to_string(),
            })
            .with_hint("use a positive interval, or leave it unset for the 100ms default"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Group, HumanDuration, ServiceCommand, Signal, TaskCommand};

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
    fn test_service_command_rejects_file_context_builtins() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new(
            "watcher",
            "./bin/handler --files ${zaz:files}",
        )];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("references ${zaz:files}"),
            "expected error mentioning ${{zaz:files}}, got: {}",
            msg
        );
        assert!(
            msg.contains("service 'watcher'"),
            "expected service name in error, got: {}",
            msg
        );
        assert!(
            msg.contains("hint:"),
            "expected hint pointing at task workaround, got: {}",
            msg
        );
    }

    #[test]
    fn test_service_command_allows_user_variables_and_root() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new(
            "watcher",
            "./bin/handler --root ${zaz:root} --lexicon ${lexicon}",
        )];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("user vars and ${zaz:root} must be allowed in service commands");
    }

    #[test]
    fn test_service_command_allows_escaped_file_builtin() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new(
            "watcher",
            r"./bin/handler --files \${zaz:files}",
        )];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("escaped ${zaz:files} must be allowed in service commands");
    }

    #[test]
    fn test_service_command_file_builtin_names_the_field() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new("watcher", "./bin/handler ${zaz:files}")];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' command references"),
            "expected the offending field named in the error, got: {}",
            msg
        );
    }

    #[test]
    fn test_cleanup_command_rejects_file_context_builtins() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.cleanup_command = Some("rm -f ${zaz:files}".to_string());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' cleanup_command references ${zaz:files}"),
            "expected cleanup_command named in error, got: {}",
            msg
        );
        assert!(
            msg.contains("hint:"),
            "expected hint pointing at task workaround, got: {}",
            msg
        );
    }

    #[test]
    fn test_cleanup_command_allows_user_variables_and_root() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.cleanup_command = Some("rm -f ${zaz:root}/${lexicon}.pid".to_string());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("user vars and ${zaz:root} must be allowed in cleanup_command");
    }

    #[test]
    fn test_empty_cleanup_command_rejected() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.cleanup_command = Some(String::new());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' has empty cleanup_command"),
            "expected empty cleanup_command error, got: {}",
            msg
        );
    }

    #[test]
    fn test_unset_cleanup_command_validates() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new("watcher", "./bin/handler")];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("an unset cleanup_command must not affect validation");
    }

    #[test]
    fn test_stop_and_kill_commands_reject_file_context_builtins() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.stop_command = Some("./ctl drain ${zaz:files}".to_string());
        service.kill_command = Some("./ctl abort ${zaz:dirs}".to_string());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' stop_command references ${zaz:files}"),
            "expected stop_command named in error, got: {}",
            msg
        );
        assert!(
            msg.contains("service 'watcher' kill_command references ${zaz:dirs}"),
            "expected kill_command named in error, got: {}",
            msg
        );
    }

    #[test]
    fn test_empty_stop_and_kill_commands_rejected() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.stop_command = Some(String::new());
        service.kill_command = Some(String::new());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' has empty stop_command"),
            "expected empty stop_command error, got: {}",
            msg
        );
        assert!(
            msg.contains("service 'watcher' has empty kill_command"),
            "expected empty kill_command error, got: {}",
            msg
        );
    }

    #[test]
    fn test_unset_stop_and_kill_commands_validate() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new("watcher", "./bin/handler")];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("unset stop_command and kill_command must not affect validation");
    }

    #[test]
    fn test_stop_command_without_a_signal_validates() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.stop_command = Some("./ctl drain".to_string());
        service.kill_command = Some("./ctl abort".to_string());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("stop_command alone must validate");
    }

    #[test]
    fn test_signal_alongside_stop_command_rejected() {
        let mut group = make_group("server");
        let mut service =
            ServiceCommand::new("watcher", "./bin/handler").with_signal(Signal::Sigint);
        service.stop_command = Some("./ctl drain".to_string());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' sets both signal and stop_command"),
            "expected conflicting stop mechanism error, got: {}",
            msg
        );
        assert!(
            msg.contains("hint:"),
            "expected hint pointing at the redundant field, got: {}",
            msg
        );
    }

    #[test]
    fn test_explicit_default_signal_alongside_stop_command_rejected() {
        let mut group = make_group("server");
        let mut service =
            ServiceCommand::new("watcher", "./bin/handler").with_signal(Signal::Sigterm);
        service.stop_command = Some("./ctl drain".to_string());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.kind.code() == "conflicting_stop_mechanism"),
            "an explicit SIGTERM is still a signal the stop_command would silence"
        );
    }

    #[test]
    fn test_signal_without_a_stop_command_validates() {
        let mut group = make_group("server");
        group.services =
            vec![ServiceCommand::new("watcher", "./bin/handler").with_signal(Signal::Sigint)];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("a signal without a stop_command must validate");
    }

    #[test]
    fn test_ready_check_rejects_file_context_builtins() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.ready_check = Some("./probe ${zaz:files}".to_string());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' ready_check references ${zaz:files}"),
            "expected ready_check named in error, got: {}",
            msg
        );
    }

    #[test]
    fn test_empty_ready_check_rejected() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.ready_check = Some(String::new());
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' has empty ready_check"),
            "expected empty ready_check error, got: {}",
            msg
        );
    }

    #[test]
    fn test_unset_ready_check_validates() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new("watcher", "./bin/handler")];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("an unset ready_check must not affect validation");
    }

    #[test]
    fn test_ready_tuning_without_a_ready_check_rejected() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.ready_poll_interval = Some(HumanDuration::from_millis(250));
        service.ready_timeout = Some(HumanDuration::from_millis(60_000));
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' sets ready_poll_interval without a ready_check"),
            "expected ready_poll_interval reported on its own, got: {}",
            msg
        );
        assert!(
            msg.contains("service 'watcher' sets ready_timeout without a ready_check"),
            "expected ready_timeout reported on its own, got: {}",
            msg
        );
        assert!(
            msg.contains("hint:"),
            "expected hint pointing at the missing ready_check, got: {}",
            msg
        );
    }

    #[test]
    fn test_ready_tuning_alongside_a_ready_check_validates() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.ready_check = Some("./probe".to_string());
        service.ready_poll_interval = Some(HumanDuration::from_millis(250));
        service.ready_timeout = Some(HumanDuration::from_millis(60_000));
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("tuning a configured ready_check must validate");
    }

    #[test]
    fn test_zero_ready_poll_interval_rejected() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.ready_check = Some("./probe".to_string());
        service.ready_poll_interval = Some(HumanDuration::from_millis(0));
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.kind.code() == "zero_ready_poll_interval"),
            "a zero interval would run the check back to back until the timeout"
        );
    }

    #[test]
    fn test_zero_ready_timeout_validates() {
        let mut group = make_group("server");
        let mut service = ServiceCommand::new("watcher", "./bin/handler");
        service.ready_check = Some("./probe".to_string());
        service.ready_timeout = Some(HumanDuration::from_millis(0));
        group.services = vec![service];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        validate(&config).expect("a zero timeout means one check then give up, which is coherent");
    }

    #[test]
    fn test_empty_service_command_rejected() {
        let mut group = make_group("server");
        group.services = vec![ServiceCommand::new("watcher", "")];
        let config = Config {
            groups: vec![group],
            ..Default::default()
        };
        let err = validate(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("service 'watcher' has empty command"),
            "expected empty command error, got: {}",
            msg
        );
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
