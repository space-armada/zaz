//! Error types for configuration parsing.

use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur when loading or parsing configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// No configuration file found.
    #[error("no configuration file found, searched: {}", format_paths(.searched))]
    NotFound { searched: Vec<PathBuf> },

    /// Failed to read configuration file.
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Unknown configuration file format.
    #[error("unknown config format for {path} (expected .toml or .json)")]
    UnknownFormat { path: PathBuf },

    /// TOML parsing error.
    #[error("TOML parse error: {message}{}", format_span(.span))]
    Toml {
        message: String,
        span: Option<Range<usize>>,
    },

    /// JSON parsing error.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// Validation errors (one or more).
    #[error("{0}")]
    Validation(#[from] ValidationErrors),
}

/// Location in source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Line number (1-indexed).
    pub line: usize,
    /// Column number (1-indexed).
    pub column: usize,
}

impl Span {
    /// Create a new span.
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }

    /// Convert a byte offset to a Span (line/column position).
    ///
    /// Both line and column are 1-indexed.
    pub fn from_byte_offset(source: &str, byte_offset: usize) -> Self {
        let mut line = 1;
        let mut column = 1;

        for (i, ch) in source.char_indices() {
            if i >= byte_offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }

        Self { line, column }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// A single validation error.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// Location in source file (if known).
    pub span: Option<Span>,
    /// Error category with details.
    pub kind: ValidationErrorKind,
    /// Suggested fix (if available).
    pub hint: Option<String>,
}

impl ValidationError {
    /// Create a new validation error.
    pub fn new(kind: ValidationErrorKind) -> Self {
        Self {
            span: None,
            kind,
            hint: None,
        }
    }

    /// Add a span to this error.
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    /// Add a hint to this error.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Get error code for JSON output.
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Location prefix
        if let Some(span) = &self.span {
            write!(f, "{}: ", span)?;
        }

        // Error message
        write!(f, "{}", self.kind)?;

        // Hint on next line, indented
        if let Some(hint) = &self.hint {
            write!(f, "\n               hint: {}", hint)?;
        }

        Ok(())
    }
}

/// Categories of validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationErrorKind {
    /// Group name is empty.
    EmptyGroupName {
        /// Index of the group in the config.
        index: usize,
    },
    /// Duplicate group name.
    DuplicateGroupName {
        /// The duplicated name.
        name: String,
        /// Index of the first occurrence.
        first_index: usize,
        /// Index of the duplicate.
        second_index: usize,
    },
    /// Group has no patterns and no commands.
    EmptyGroup {
        /// Name of the empty group.
        name: String,
    },
    /// Unknown dependency reference.
    UnknownDependency {
        /// Name of the group with the bad dependency.
        group: String,
        /// Name of the unknown dependency.
        dependency: String,
    },
    /// Group depends on itself.
    SelfDependency {
        /// Name of the group.
        group: String,
    },
    /// Dependency cycle detected.
    DependencyCycle {
        /// The cycle path.
        cycle: Vec<String>,
    },
    /// Invalid glob pattern.
    InvalidPattern {
        /// Name of the group.
        group: String,
        /// The invalid pattern.
        pattern: String,
        /// Error message from the glob parser.
        error: String,
    },
    /// Invalid ignore pattern.
    InvalidIgnorePattern {
        /// Name of the group.
        group: String,
        /// The invalid pattern.
        pattern: String,
        /// Error message from the glob parser.
        error: String,
    },
    /// Task has empty command.
    EmptyTaskCommand {
        /// Name of the group.
        group: String,
        /// Name of the task.
        task: String,
    },
    /// Duplicate task name.
    DuplicateTaskName {
        /// Name of the group.
        group: String,
        /// The duplicated name.
        name: String,
        /// Whether the name was explicitly set.
        is_explicit: bool,
    },
    /// Daemon has empty command.
    EmptyDaemonCommand {
        /// Name of the group.
        group: String,
        /// Name of the daemon.
        daemon: String,
    },
    /// Duplicate daemon name.
    DuplicateDaemonName {
        /// Name of the group.
        group: String,
        /// The duplicated name.
        name: String,
        /// Whether the name was explicitly set.
        is_explicit: bool,
    },
}

impl ValidationErrorKind {
    /// Get error code for JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyGroupName { .. } => "empty_group_name",
            Self::DuplicateGroupName { .. } => "duplicate_group_name",
            Self::EmptyGroup { .. } => "empty_group",
            Self::UnknownDependency { .. } => "unknown_dependency",
            Self::SelfDependency { .. } => "self_dependency",
            Self::DependencyCycle { .. } => "dependency_cycle",
            Self::InvalidPattern { .. } => "invalid_pattern",
            Self::InvalidIgnorePattern { .. } => "invalid_ignore_pattern",
            Self::EmptyTaskCommand { .. } => "empty_task_command",
            Self::DuplicateTaskName { .. } => "duplicate_task_name",
            Self::EmptyDaemonCommand { .. } => "empty_daemon_command",
            Self::DuplicateDaemonName { .. } => "duplicate_daemon_name",
        }
    }
}

impl fmt::Display for ValidationErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroupName { index } => {
                write!(f, "group[{}]: name cannot be empty", index)
            }
            Self::DuplicateGroupName {
                name,
                first_index,
                second_index,
            } => {
                write!(
                    f,
                    "group[{}]: duplicate name '{}' (first defined at group[{}])",
                    second_index, name, first_index
                )
            }
            Self::EmptyGroup { name } => {
                write!(f, "group '{}': has no patterns and no commands", name)
            }
            Self::UnknownDependency { group, dependency } => {
                write!(
                    f,
                    "group '{}': depends_on references unknown group '{}'",
                    group, dependency
                )
            }
            Self::SelfDependency { group } => {
                write!(f, "group '{}': cannot depend on itself", group)
            }
            Self::DependencyCycle { cycle } => {
                write!(f, "dependency cycle detected: {}", cycle.join(" -> "))
            }
            Self::InvalidPattern {
                group,
                pattern,
                error,
            } => {
                write!(
                    f,
                    "group '{}': invalid pattern '{}': {}",
                    group, pattern, error
                )
            }
            Self::InvalidIgnorePattern {
                group,
                pattern,
                error,
            } => {
                write!(
                    f,
                    "group '{}': invalid ignore pattern '{}': {}",
                    group, pattern, error
                )
            }
            Self::EmptyTaskCommand { group, task } => {
                write!(f, "group '{}': task '{}' has empty command", group, task)
            }
            Self::DuplicateTaskName {
                group,
                name,
                is_explicit,
            } => {
                let hint = if *is_explicit {
                    ""
                } else {
                    " (use explicit 'name' field to disambiguate)"
                };
                write!(
                    f,
                    "group '{}': duplicate task name '{}'{}",
                    group, name, hint
                )
            }
            Self::EmptyDaemonCommand { group, daemon } => {
                write!(
                    f,
                    "group '{}': daemon '{}' has empty command",
                    group, daemon
                )
            }
            Self::DuplicateDaemonName {
                group,
                name,
                is_explicit,
            } => {
                let hint = if *is_explicit {
                    ""
                } else {
                    " (use explicit 'name' field to disambiguate)"
                };
                write!(
                    f,
                    "group '{}': duplicate daemon name '{}'{}",
                    group, name, hint
                )
            }
        }
    }
}

/// Collection of validation errors.
#[derive(Debug, Clone, Default)]
pub struct ValidationErrors {
    errors: Vec<ValidationError>,
}

impl ValidationErrors {
    /// Create a new empty collection.
    pub fn new() -> Self {
        Self { errors: Vec::new() }
    }

    /// Check if there are no errors.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// Get the number of errors.
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Add an error to the collection.
    pub fn push(&mut self, error: ValidationError) {
        self.errors.push(error);
    }

    /// Get an iterator over the errors.
    pub fn iter(&self) -> impl Iterator<Item = &ValidationError> {
        self.errors.iter()
    }

    /// Convert to a Result, returning Ok if empty.
    pub fn into_result(self) -> Result<(), Self> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "configuration validation failed:")?;
        for error in &self.errors {
            write!(f, "\n{}", error)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

impl IntoIterator for ValidationErrors {
    type Item = ValidationError;
    type IntoIter = std::vec::IntoIter<ValidationError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.into_iter()
    }
}

impl<'a> IntoIterator for &'a ValidationErrors {
    type Item = &'a ValidationError;
    type IntoIter = std::slice::Iter<'a, ValidationError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.iter()
    }
}

fn format_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_span(span: &Option<Range<usize>>) -> String {
    match span {
        Some(range) => format!(" at bytes {}..{}", range.start, range.end),
        None => String::new(),
    }
}

impl ConfigError {
    /// Check if this is a "not found" error.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ConfigError::NotFound { .. })
    }

    /// Check if this is a validation error.
    pub fn is_validation(&self) -> bool {
        matches!(self, ConfigError::Validation(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_found_error_display() {
        let err = ConfigError::NotFound {
            searched: vec![PathBuf::from("zaz.toml"), PathBuf::from("zaz.json")],
        };
        let msg = err.to_string();
        assert!(msg.contains("zaz.toml"));
        assert!(msg.contains("zaz.json"));
    }

    #[test]
    fn test_toml_error_with_span() {
        let err = ConfigError::Toml {
            message: "expected value".to_string(),
            span: Some(10..15),
        };
        let msg = err.to_string();
        assert!(msg.contains("expected value"));
        assert!(msg.contains("bytes 10..15"));
    }

    #[test]
    fn test_validation_error() {
        let mut errors = ValidationErrors::new();
        errors.push(ValidationError::new(
            ValidationErrorKind::DuplicateGroupName {
                name: "foo".to_string(),
                first_index: 0,
                second_index: 1,
            },
        ));
        let err = ConfigError::Validation(errors);
        assert!(err.is_validation());
        assert!(err.to_string().contains("duplicate name"));
    }

    #[test]
    fn test_validation_error_with_span_and_hint() {
        let error = ValidationError::new(ValidationErrorKind::UnknownDependency {
            group: "frontend".to_string(),
            dependency: "star".to_string(),
        })
        .with_span(Span::new(23, 1))
        .with_hint("Available groups are: backend, protobuf");

        let msg = error.to_string();
        assert!(msg.contains("23:1"));
        assert!(msg.contains("unknown group 'star'"));
        assert!(msg.contains("hint: Available groups are"));
    }

    #[test]
    fn test_validation_error_codes() {
        assert_eq!(
            ValidationErrorKind::EmptyGroupName { index: 0 }.code(),
            "empty_group_name"
        );
        assert_eq!(
            ValidationErrorKind::DependencyCycle {
                cycle: vec!["a".to_string()]
            }
            .code(),
            "dependency_cycle"
        );
    }

    #[test]
    fn test_validation_errors_collection() {
        let mut errors = ValidationErrors::new();
        assert!(errors.is_empty());
        assert_eq!(errors.len(), 0);

        errors.push(ValidationError::new(ValidationErrorKind::EmptyGroupName {
            index: 0,
        }));
        errors.push(ValidationError::new(ValidationErrorKind::EmptyGroupName {
            index: 1,
        }));

        assert!(!errors.is_empty());
        assert_eq!(errors.len(), 2);
        assert_eq!(errors.iter().count(), 2);

        // Test into_result
        let result = errors.into_result();
        assert!(result.is_err());
    }
}
