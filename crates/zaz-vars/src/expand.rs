//! Variable expansion implementation.

use crate::VarError;
use std::collections::HashMap;
use std::path::PathBuf;

/// Context for variable expansion, providing values for built-in and custom variables.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Custom user-defined variables.
    pub variables: HashMap<String, String>,

    /// Modified files (for `${mods}`).
    pub mods: Vec<PathBuf>,

    /// Config file directory (for `${confdir}`).
    pub confdir: Option<PathBuf>,
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
    pub fn with_mods(mut self, mods: Vec<PathBuf>) -> Self {
        self.mods = mods;
        self
    }

    /// Set config directory.
    pub fn with_confdir(mut self, confdir: PathBuf) -> Self {
        self.confdir = Some(confdir);
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
            "mods" => Ok(self.format_paths(&self.context.mods)),
            "dirmods" => Ok(self.format_dirmods()),
            "confdir" => self
                .context
                .confdir
                .as_ref()
                .map(|p| p.display().to_string())
                .ok_or_else(|| VarError::Undefined("confdir".to_string())),
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

    /// Get unique directories from mods and format them.
    fn format_dirmods(&self) -> String {
        let mut dirs: Vec<_> = self
            .context
            .mods
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
}
