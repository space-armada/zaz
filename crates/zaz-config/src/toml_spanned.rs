//! TOML-specific schema with span information for error reporting.
//!
//! This module provides types that mirror the main schema but use toml::Spanned
//! to track source positions for better error messages.

use crate::error::Span;
use crate::schema::{Config, DaemonCommand, Group, Settings, TaskCommand};
use serde::Deserialize;
use std::collections::HashMap;
use std::ops::Range;

/// Span information extracted from TOML parsing.
#[derive(Debug, Clone, Default)]
pub struct SpanInfo {
    /// Spans for group names, keyed by group index.
    pub group_names: HashMap<usize, Range<usize>>,
    /// Spans for dependency references, keyed by (group_index, dep_index).
    pub dependencies: HashMap<(usize, usize), Range<usize>>,
}

impl SpanInfo {
    /// Get a Span for a group name, converting byte offset to line/column.
    pub fn group_name_span(&self, source: &str, group_index: usize) -> Option<Span> {
        self.group_names
            .get(&group_index)
            .map(|range| Span::from_byte_offset(source, range.start))
    }

    /// Get a Span for a dependency reference, converting byte offset to line/column.
    pub fn dependency_span(
        &self,
        source: &str,
        group_index: usize,
        dep_index: usize,
    ) -> Option<Span> {
        self.dependencies
            .get(&(group_index, dep_index))
            .map(|range| Span::from_byte_offset(source, range.start))
    }
}

/// Internal TOML config with spanned group definitions.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpannedConfig {
    settings: Settings,
    variables: HashMap<String, String>,
    #[serde(alias = "group")]
    groups: Vec<SpannedGroup>,
}

/// Internal group type with spanned name and dependencies.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct SpannedGroup {
    name: toml::Spanned<String>,
    patterns: Vec<String>,
    ignore: Vec<String>,
    depends_on: Vec<toml::Spanned<String>>,
    working_dir: Option<String>,
    env: HashMap<String, String>,
    #[serde(alias = "task")]
    tasks: Vec<TaskCommand>,
    #[serde(alias = "daemon")]
    daemons: Vec<DaemonCommand>,
}

impl Default for SpannedGroup {
    fn default() -> Self {
        Self {
            name: toml::Spanned::new(0..0, String::new()),
            patterns: Vec::new(),
            ignore: Vec::new(),
            depends_on: Vec::new(),
            working_dir: None,
            env: HashMap::new(),
            tasks: Vec::new(),
            daemons: Vec::new(),
        }
    }
}

/// Parse TOML and extract both the config and span information.
pub fn parse_toml_with_spans(
    contents: &str,
) -> Result<(Config, SpanInfo), crate::error::ConfigError> {
    let spanned: SpannedConfig =
        toml::from_str(contents).map_err(|e| crate::error::ConfigError::Toml {
            message: e.message().to_string(),
            span: e.span(),
        })?;

    let mut span_info = SpanInfo::default();

    // Extract spans and convert to regular Config
    let groups: Vec<Group> = spanned
        .groups
        .into_iter()
        .enumerate()
        .map(|(i, sg)| {
            // Store group name span
            span_info
                .group_names
                .insert(i, sg.name.span().start..sg.name.span().end);

            // Store dependency spans
            for (j, dep) in sg.depends_on.iter().enumerate() {
                span_info
                    .dependencies
                    .insert((i, j), dep.span().start..dep.span().end);
            }

            Group {
                name: sg.name.into_inner(),
                patterns: sg.patterns,
                ignore: sg.ignore,
                depends_on: sg.depends_on.into_iter().map(|s| s.into_inner()).collect(),
                working_dir: sg.working_dir,
                env: sg.env,
                tasks: sg.tasks,
                daemons: sg.daemons,
            }
        })
        .collect();

    let config = Config {
        settings: spanned.settings,
        variables: spanned.variables,
        groups,
    };

    Ok((config, span_info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_with_spans() {
        let toml = r#"
[[group]]
name = "backend"
patterns = ["*.go"]
depends_on = ["frontend"]

[[group]]
name = "frontend"
patterns = ["*.ts"]
"#;
        let (config, spans) = parse_toml_with_spans(toml).unwrap();
        assert_eq!(config.groups.len(), 2);
        assert_eq!(config.groups[0].name, "backend");

        // Verify spans were captured
        assert!(spans.group_names.contains_key(&0));
        assert!(spans.group_names.contains_key(&1));
        assert!(spans.dependencies.contains_key(&(0, 0)));

        // Convert to line/column
        let span = spans.group_name_span(toml, 0).unwrap();
        assert!(span.line > 0);
        assert!(span.column > 0);
    }

    #[test]
    fn test_span_byte_to_line_column() {
        let source = "line1\nname = \"test\"\nline3";
        // "name" starts at byte 6 (after "line1\n")
        let span = Span::from_byte_offset(source, 6);
        assert_eq!(span.line, 2);
        assert_eq!(span.column, 1);
    }
}
