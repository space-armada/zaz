//! Variable expansion implementation.

use crate::VarError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Context for variable expansion, providing values for built-in and custom variables.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Custom user-defined variables.
    pub variables: HashMap<String, String>,

    /// Modified files (for `${zaz:files}`).
    pub files: Vec<PathBuf>,

    /// Config file directory (for `${zaz:root}`).
    pub root: Option<PathBuf>,
}

impl Context {
    /// Create a new empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set custom variables from a map.
    pub fn with_variables(mut self, vars: HashMap<String, String>) -> Self {
        self.variables = vars;
        self
    }

    /// Set modified files.
    pub fn with_files(mut self, files: Vec<PathBuf>) -> Self {
        self.files = files;
        self
    }

    /// Set config root directory.
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = Some(root);
        self
    }
}

/// Variable expander that processes `${var}` syntax.
pub struct Expander<'a> {
    context: &'a Context,
}

impl<'a> Expander<'a> {
    /// Create a new expander with the given context.
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }

    /// Expand all variables in the input string.
    pub fn expand(&self, input: &str) -> Result<String, VarError> {
        let mut result = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\\' {
                // Check for escaped dollar sign
                if chars.peek() == Some(&'$') {
                    chars.next();
                    result.push('$');
                } else {
                    result.push(c);
                }
            } else if c == '$' && chars.peek() == Some(&'{') {
                chars.next(); // consume '{'

                // Find the closing brace
                let mut var_name = String::new();
                let mut found_close = false;

                for ch in chars.by_ref() {
                    if ch == '}' {
                        found_close = true;
                        break;
                    }
                    var_name.push(ch);
                }

                if !found_close {
                    return Err(VarError::Malformed(result.len()));
                }

                let value = self.resolve(&var_name)?;
                result.push_str(&value);
            } else {
                result.push(c);
            }
        }

        Ok(result)
    }

    /// Resolve a variable name to its value.
    fn resolve(&self, name: &str) -> Result<String, VarError> {
        match name {
            // Built-in zaz: namespaced variables
            "zaz:files" => Ok(self.format_paths(&self.context.files)),
            "zaz:dirs" => Ok(self.format_dirs()),
            "zaz:root" => self
                .context
                .root
                .as_ref()
                .map(|p| p.display().to_string())
                .ok_or_else(|| VarError::Undefined("zaz:root".to_string())),
            "zaz:prefix" => Ok(self.format_prefix()),
            // User-defined variables
            _ => self
                .context
                .variables
                .get(name)
                .cloned()
                .ok_or_else(|| VarError::Undefined(name.to_string())),
        }
    }

    /// Format a list of paths as a space-separated, shell-safe string.
    fn format_paths(&self, paths: &[PathBuf]) -> String {
        paths
            .iter()
            .map(|p| shell_quote(&p.display().to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Get unique directories from files and format them.
    fn format_dirs(&self) -> String {
        let mut dirs: Vec<_> = self
            .context
            .files
            .iter()
            .filter_map(|p| p.parent())
            .map(|p| p.to_path_buf())
            .collect();

        dirs.sort();
        dirs.dedup();

        dirs.iter()
            .map(|p| shell_quote(&p.display().to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Find the common prefix directory of all modified files.
    fn format_prefix(&self) -> String {
        if self.context.files.is_empty() {
            return String::new();
        }

        let prefix = common_path_prefix(&self.context.files);
        shell_quote(&prefix.display().to_string())
    }
}

/// Find the common path prefix of a list of paths.
fn common_path_prefix(paths: &[PathBuf]) -> PathBuf {
    if paths.is_empty() {
        return PathBuf::new();
    }

    if paths.len() == 1 {
        // For a single file, return its parent directory
        return paths[0].parent().unwrap_or(Path::new("")).to_path_buf();
    }

    // Get all path components
    let components: Vec<Vec<_>> = paths.iter().map(|p| p.components().collect()).collect();

    // Find common prefix
    let mut prefix = PathBuf::new();
    let min_len = components.iter().map(|c| c.len()).min().unwrap_or(0);

    for i in 0..min_len {
        let first = &components[0][i];
        if components.iter().all(|c| &c[i] == first) {
            prefix.push(first);
        } else {
            break;
        }
    }

    // If the prefix is a file (not a directory), use its parent
    if paths.iter().any(|p| p == &prefix) {
        prefix.parent().unwrap_or(Path::new("")).to_path_buf()
    } else {
        prefix
    }
}

/// Quote a string for shell safety.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_simple() {
        let mut ctx = Context::new();
        ctx.variables.insert("foo".to_string(), "bar".to_string());

        let expander = Expander::new(&ctx);
        assert_eq!(expander.expand("hello ${foo}").unwrap(), "hello bar");
    }

    #[test]
    fn test_expand_escaped() {
        let ctx = Context::new();
        let expander = Expander::new(&ctx);
        assert_eq!(expander.expand("\\${foo}").unwrap(), "${foo}");
    }

    #[test]
    fn test_expand_undefined() {
        let ctx = Context::new();
        let expander = Expander::new(&ctx);
        assert!(expander.expand("${undefined}").is_err());
    }

    #[test]
    fn test_zaz_files() {
        let ctx = Context::new().with_files(vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
        ]);
        let expander = Expander::new(&ctx);
        assert_eq!(
            expander.expand("${zaz:files}").unwrap(),
            "src/main.rs src/lib.rs"
        );
    }

    #[test]
    fn test_zaz_dirs() {
        let ctx = Context::new().with_files(vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("tests/test.rs"),
        ]);
        let expander = Expander::new(&ctx);
        assert_eq!(expander.expand("${zaz:dirs}").unwrap(), "src tests");
    }

    #[test]
    fn test_zaz_root() {
        let ctx = Context::new().with_root(PathBuf::from("/home/user/project"));
        let expander = Expander::new(&ctx);
        assert_eq!(
            expander.expand("${zaz:root}").unwrap(),
            "/home/user/project"
        );
    }

    #[test]
    fn test_zaz_prefix() {
        let ctx = Context::new().with_files(vec![
            PathBuf::from("src/foo/a.rs"),
            PathBuf::from("src/foo/b.rs"),
            PathBuf::from("src/foo/bar/c.rs"),
        ]);
        let expander = Expander::new(&ctx);
        assert_eq!(expander.expand("${zaz:prefix}").unwrap(), "src/foo");
    }

    #[test]
    fn test_zaz_prefix_single_file() {
        let ctx = Context::new().with_files(vec![PathBuf::from("src/main.rs")]);
        let expander = Expander::new(&ctx);
        assert_eq!(expander.expand("${zaz:prefix}").unwrap(), "src");
    }

    #[test]
    fn test_common_path_prefix() {
        let paths = vec![PathBuf::from("src/foo/a.rs"), PathBuf::from("src/foo/b.rs")];
        assert_eq!(common_path_prefix(&paths), PathBuf::from("src/foo"));

        let paths = vec![PathBuf::from("src/foo/a.rs"), PathBuf::from("src/bar/b.rs")];
        assert_eq!(common_path_prefix(&paths), PathBuf::from("src"));

        let paths = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        assert_eq!(common_path_prefix(&paths), PathBuf::from(""));
    }
}
