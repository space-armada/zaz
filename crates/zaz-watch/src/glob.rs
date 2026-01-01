//! Glob pattern matching.

use crate::WatchError;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

/// A compiled set of glob patterns for matching file paths.
#[derive(Debug, Clone)]
pub struct PatternSet {
    include: GlobSet,
    exclude: GlobSet,
}

impl PatternSet {
    /// Create a new pattern set from include and exclude patterns.
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, WatchError> {
        let include = build_globset(include)?;
        let exclude = build_globset(exclude)?;
        Ok(Self { include, exclude })
    }

    /// Check if a path matches this pattern set (included and not excluded).
    pub fn matches(&self, path: &Path) -> bool {
        self.include.is_match(path) && !self.exclude.is_match(path)
    }

    /// Check if this pattern set has any include patterns.
    pub fn is_empty(&self) -> bool {
        self.include.is_empty()
    }
}

impl Default for PatternSet {
    fn default() -> Self {
        Self {
            include: GlobSet::empty(),
            exclude: GlobSet::empty(),
        }
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, WatchError> {
    let mut builder = GlobSetBuilder::new();

    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| WatchError::InvalidPattern {
            pattern: pattern.clone(),
            source: e,
        })?;
        builder.add(glob);
    }

    builder.build().map_err(|e| WatchError::InvalidPattern {
        pattern: "<combined>".to_string(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_matching() {
        let patterns =
            PatternSet::new(&["**/*.rs".to_string()], &["**/target/**".to_string()]).unwrap();

        assert!(patterns.matches(Path::new("src/main.rs")));
        assert!(patterns.matches(Path::new("crates/foo/src/lib.rs")));
        assert!(!patterns.matches(Path::new("target/debug/main.rs")));
        assert!(!patterns.matches(Path::new("README.md")));
    }
}
