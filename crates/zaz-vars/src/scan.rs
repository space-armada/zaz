//! Read-only inspection of `${var}` references in a command string.
//!
//! Used by validators that need to know which variables a command references
//! without resolving them.

/// Built-in variables that require file-change context (a non-empty `files`
/// list in the expansion context).
pub const FILE_CONTEXT_BUILTINS: &[&str] = &["zaz:files", "zaz:dirs", "zaz:prefix"];

/// Iterate over every `${name}` reference in `input`, returning the bare names.
///
/// Mirrors the expansion grammar: `\$` escapes a dollar sign, malformed
/// references with no closing brace are silently skipped (the expander would
/// error on them separately).
pub fn references(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];

        if c == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            i += 2;
            continue;
        }

        if c == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            if let Some(rel_end) = input[start..].find('}') {
                let end = start + rel_end;
                out.push(&input[start..end]);
                i = end + 1;
                continue;
            }
        }

        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_reference() {
        assert_eq!(references("hello ${name}"), vec!["name"]);
    }

    #[test]
    fn extracts_multiple_references() {
        assert_eq!(
            references("${a}/${b}/${zaz:files}"),
            vec!["a", "b", "zaz:files"]
        );
    }

    #[test]
    fn skips_escaped_dollar() {
        assert_eq!(references(r"\${literal} ${real}"), vec!["real"]);
    }

    #[test]
    fn skips_unterminated_reference() {
        assert_eq!(references("${unterminated"), Vec::<&str>::new());
    }

    #[test]
    fn empty_for_plain_text() {
        assert_eq!(references("no variables here"), Vec::<&str>::new());
    }
}
