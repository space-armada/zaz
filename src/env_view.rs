//! The text view of the loaded environment behind `zaz env`.
//!
//! The report holds key names and sources, never values. A failed source's error text is the one
//! place a command's stderr can appear.

use anyhow::{bail, Result};
use zaz_daemon::{
    EnvReport, EnvSourceKind, EnvSourceReport, EnvStart, GroupEnvReport, ProcessEnvReport,
};

/// Render `report` for a terminal, narrowed to `group` when one is named.
///
/// A group that the report does not hold is an error, unless nothing is loaded yet. The caller
/// prints the returned text.
pub(crate) fn render(report: &EnvReport, group: Option<&str>) -> Result<String> {
    let mut out = String::new();

    out.push_str("Loaded environment of the running config (key names, not values)\n");

    if let Some(project) = &report.project {
        out.push_str(&format!("Project: {project}\n"));
    }

    if let Some(error) = &report.load_error {
        out.push_str(&format!(
            "Load error: [{}] first load failed, nothing is built, so no keys are loaded\n",
            error.code
        ));
        push_lines(&mut out, &error.message, "  ");
        if let Some(hint) = &error.hint {
            out.push_str(&format!("  hint: {hint}\n"));
        }
        out.push_str("  fix the config and run `zaz reload`, or run `zaz stop`\n");

        return Ok(out);
    }

    if report.groups.is_empty() {
        out.push_str("The daemon is still loading its configuration, so no keys are loaded yet.\n");

        return Ok(out);
    }

    if let Some(error) = &report.refresh_error {
        out.push_str(&format!(
            "Environment refresh error: [{}] the last refresh of a watched source failed, running \
             on the previous environment shown below\n",
            error.code
        ));
        push_lines(&mut out, &error.message, "  ");
        if let Some(hint) = &error.hint {
            out.push_str(&format!("  hint: {hint}\n"));
        }
        out.push_str("  fix the source, or run `zaz reload` to evaluate everything again\n");
    }

    let shown: Vec<&GroupEnvReport> = report
        .groups
        .iter()
        .filter(|candidate| group.map_or(true, |name| candidate.name == name))
        .collect();

    if let Some(name) = group {
        if shown.is_empty() {
            bail!("no group named '{name}' in the running config");
        }
    }

    if !report.dropped_inherited.is_empty() {
        out.push_str(&format!(
            "Inherited variables the base environment drops ({}): {}\n",
            report.dropped_inherited.len(),
            report.dropped_inherited.join(", ")
        ));
    }

    for group in shown {
        out.push('\n');
        push_group(&mut out, group);
    }

    Ok(out)
}

fn push_group(out: &mut String, group: &GroupEnvReport) {
    out.push_str(&format!(
        "group {} (starts from {})\n",
        group.name,
        start_label(group.starts_from)
    ));
    push_sources(out, &group.sources, "  ");

    for (kind, processes) in [("task", &group.tasks), ("service", &group.services)] {
        for process in processes {
            push_process(out, kind, process, group);
        }
    }
}

fn push_process(out: &mut String, kind: &str, process: &ProcessEnvReport, group: &GroupEnvReport) {
    let own = process
        .sources
        .get(group.sources.len()..)
        .unwrap_or_default();
    let starts = if process.starts_from == group.starts_from {
        String::new()
    } else {
        format!(" (starts from {})", start_label(process.starts_from))
    };

    out.push_str(&format!("  {kind} {}{starts}\n", process.name));

    if own.is_empty() {
        out.push_str("    no sources of its own\n");
    } else {
        push_sources(out, own, "    ");
    }
}

fn push_sources(out: &mut String, sources: &[EnvSourceReport], indent: &str) {
    if sources.is_empty() {
        out.push_str(&format!("{indent}no sources\n"));
    }

    for source in sources {
        out.push_str(&format!("{indent}{}\n", source_line(source)));
    }
}

fn source_line(source: &EnvSourceReport) -> String {
    let origin = match (&source.origin, source.kind) {
        (Some(origin), EnvSourceKind::File) => format!(" ({origin})"),
        (Some(origin), EnvSourceKind::Command) => format!(" (`{origin}`)"),
        _ => String::new(),
    };

    if source.produced_no_keys {
        return format!("{}{origin}: produced no keys", source.field);
    }

    let mut parts = Vec::new();

    if !source.set.is_empty() {
        parts.push(format!("sets {}", source.set.join(", ")));
    }
    if !source.unset.is_empty() {
        parts.push(format!("unsets {}", source.unset.join(", ")));
    }

    format!("{}{origin}: {}", source.field, parts.join("; "))
}

fn start_label(start: EnvStart) -> &'static str {
    match start {
        EnvStart::Daemon => "the daemon's environment",
        EnvStart::Base => "the base environment",
    }
}

fn push_lines(out: &mut String, text: &str, indent: &str) {
    for line in text.lines() {
        out.push_str(&format!("{indent}{line}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaz_daemon::{DaemonStatus, EnvLevel, LoadError};

    fn source(field: &str, kind: EnvSourceKind, set: &[&str], unset: &[&str]) -> EnvSourceReport {
        EnvSourceReport {
            level: EnvLevel::Group,
            field: field.to_string(),
            kind,
            origin: match kind {
                EnvSourceKind::Inline => None,
                _ => Some("origin".to_string()),
            },
            set: set.iter().map(|s| s.to_string()).collect(),
            unset: unset.iter().map(|s| s.to_string()).collect(),
            produced_no_keys: set.is_empty() && unset.is_empty(),
        }
    }

    fn report() -> EnvReport {
        let group_sources = vec![
            source("group 'a' env_file", EnvSourceKind::File, &["A", "B"], &[]),
            source("group 'a' env_command", EnvSourceKind::Command, &[], &[]),
        ];
        let mut task_sources = group_sources.clone();
        task_sources.push(source(
            "group 'a' task 't' env",
            EnvSourceKind::Inline,
            &["T"],
            &["A"],
        ));

        EnvReport {
            project: None,
            status: DaemonStatus::Running,
            load_error: None,
            refresh_error: None,
            dropped_inherited: vec!["FOO".to_string(), "BAR".to_string()],
            groups: vec![
                GroupEnvReport {
                    name: "a".to_string(),
                    starts_from: EnvStart::Base,
                    sources: group_sources,
                    tasks: vec![ProcessEnvReport {
                        name: "t".to_string(),
                        starts_from: EnvStart::Base,
                        sources: task_sources,
                    }],
                    services: Vec::new(),
                },
                GroupEnvReport {
                    name: "b".to_string(),
                    starts_from: EnvStart::Daemon,
                    sources: Vec::new(),
                    tasks: Vec::new(),
                    services: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn the_view_is_labeled_and_lists_keys_sources_and_dropped_names() {
        let text = render(&report(), None).unwrap();

        assert!(text.starts_with("Loaded environment of the running config"));
        assert!(text.contains("Inherited variables the base environment drops (2): FOO, BAR"));
        assert!(text.contains("group a (starts from the base environment)"));
        assert!(text.contains("  group 'a' env_file (origin): sets A, B"));
        assert!(text.contains("  group 'a' env_command (`origin`): produced no keys"));
        assert!(text.contains("group b (starts from the daemon's environment)"));
        assert!(text.contains("no sources"));
    }

    #[test]
    fn a_process_prints_only_the_sources_beyond_its_groups() {
        let text = render(&report(), Some("a")).unwrap();

        assert!(text.contains("  task t\n    group 'a' task 't' env: sets T; unsets A\n"));
        assert_eq!(text.matches("group 'a' env_file").count(), 1);
        assert!(!text.contains("group b"));
    }

    #[test]
    fn an_unknown_group_is_an_error() {
        let err = render(&report(), Some("zzz")).unwrap_err();

        assert_eq!(
            err.to_string(),
            "no group named 'zzz' in the running config"
        );
    }

    #[test]
    fn a_load_error_prints_no_keys_and_a_starting_daemon_says_so() {
        let mut failed = EnvReport {
            status: DaemonStatus::Starting,
            ..Default::default()
        };
        failed.load_error =
            Some(LoadError::new("env_file_missing", "no such file").with_hint("fix it"));
        let text = render(&failed, Some("whatever")).unwrap();

        assert!(text.contains("Load error: [env_file_missing]"));
        assert!(text.contains("hint: fix it"));
        assert!(!text.contains("group "));

        let starting = EnvReport {
            status: DaemonStatus::Starting,
            ..Default::default()
        };
        let text = render(&starting, None).unwrap();

        assert!(text.contains("still loading"));
    }

    #[test]
    fn a_refresh_failure_prints_above_the_previous_environment() {
        let mut stale = report();
        stale.refresh_error = Some(LoadError::new("env_command_failed", "boom"));
        stale.project = Some("api".to_string());
        let text = render(&stale, None).unwrap();

        let error = text
            .find("Environment refresh error: [env_command_failed]")
            .unwrap();
        let groups = text.find("group a").unwrap();
        assert!(error < groups);
        assert!(text.contains("Project: api"));
    }
}
