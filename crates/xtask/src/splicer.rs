//! Splices generated content into a markdown file between matching
//! `<!-- BEGIN: <name> -->` / `<!-- END: <name> -->` marker pairs.

use anyhow::{anyhow, Result};

const BEGIN_PREFIX: &str = "<!-- BEGIN: ";
const END_PREFIX: &str = "<!-- END: ";
const MARKER_SUFFIX: &str = " -->";

/// Replace the body of the marker block named `name` in `text` with
/// `content`. Hand-written prose outside the markers is preserved verbatim.
///
/// `content` is inserted as-is; the caller is responsible for surrounding
/// blank lines if a particular layout is desired.
pub fn splice(name: &str, content: &str, text: &str) -> Result<String> {
    let begin_marker = format!("{BEGIN_PREFIX}{name}{MARKER_SUFFIX}");
    let end_marker = format!("{END_PREFIX}{name}{MARKER_SUFFIX}");

    let begin_idx = text
        .find(&begin_marker)
        .ok_or_else(|| anyhow!("missing begin marker: {begin_marker}"))?;
    let after_begin = begin_idx + begin_marker.len();

    let end_search_offset = after_begin;
    let rel_end_idx = text[end_search_offset..]
        .find(&end_marker)
        .ok_or_else(|| anyhow!("missing end marker: {end_marker} (after begin)"))?;
    let end_idx = end_search_offset + rel_end_idx;

    if let Some(extra_begin) = text[after_begin..end_idx].find(&begin_marker) {
        return Err(anyhow!(
            "duplicate begin marker for {name} at offset {}",
            after_begin + extra_begin
        ));
    }

    let body = format_body(content);
    let mut out = String::with_capacity(text.len() + body.len());
    out.push_str(&text[..after_begin]);
    out.push_str(&body);
    out.push_str(&text[end_idx..]);
    Ok(out)
}

fn format_body(content: &str) -> String {
    let trimmed = content.trim_matches('\n');
    if trimmed.is_empty() {
        "\n".to_string()
    } else {
        format!("\n{trimmed}\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_replaces_block_body() {
        let text = "intro\n<!-- BEGIN: foo -->\nold\n<!-- END: foo -->\noutro\n";
        let out = splice("foo", "new", text).unwrap();
        assert_eq!(
            out,
            "intro\n<!-- BEGIN: foo -->\nnew\n\n<!-- END: foo -->\noutro\n"
        );
    }

    #[test]
    fn splice_preserves_hand_written_prose_around_markers() {
        let text = "\
# Heading

Hand-written intro.

<!-- BEGIN: zaz -->
old generated
<!-- END: zaz -->

Hand-written outro.
";
        let out = splice("zaz", "new generated", text).unwrap();
        assert!(out.contains("# Heading"));
        assert!(out.contains("Hand-written intro."));
        assert!(out.contains("Hand-written outro."));
        assert!(out.contains("\nnew generated\n"));
        assert!(!out.contains("old generated"));
    }

    #[test]
    fn splice_handles_multiple_marker_pairs_independently() {
        let text = "\
<!-- BEGIN: a -->
old a
<!-- END: a -->

<!-- BEGIN: b -->
old b
<!-- END: b -->
";
        let out = splice("a", "new a", text).unwrap();
        assert!(out.contains("\nnew a\n"));
        assert!(out.contains("old b"));
        let out2 = splice("b", "new b", &out).unwrap();
        assert!(out2.contains("\nnew a\n"));
        assert!(out2.contains("\nnew b\n"));
    }

    #[test]
    fn splice_does_not_mistake_other_markers_for_target() {
        let text = "\
<!-- BEGIN: zaz -->
old
<!-- END: zaz -->

<!-- BEGIN: zaz task -->
keep me
<!-- END: zaz task -->
";
        let out = splice("zaz", "new", text).unwrap();
        assert!(out.contains("\nnew\n"));
        assert!(out.contains("keep me"));
    }

    #[test]
    fn splice_errors_on_missing_begin_marker() {
        let text = "no markers here\n";
        let err = splice("zaz", "x", text).unwrap_err().to_string();
        assert!(err.contains("missing begin marker"));
    }

    #[test]
    fn splice_errors_on_missing_end_marker() {
        let text = "<!-- BEGIN: zaz -->\nbody\nno end\n";
        let err = splice("zaz", "x", text).unwrap_err().to_string();
        assert!(err.contains("missing end marker"));
    }

    #[test]
    fn splice_errors_on_duplicate_begin_marker_in_block() {
        let text = "\
<!-- BEGIN: zaz -->
<!-- BEGIN: zaz -->
<!-- END: zaz -->
";
        let err = splice("zaz", "x", text).unwrap_err().to_string();
        assert!(err.contains("duplicate begin marker"));
    }

    #[test]
    fn splice_normalizes_empty_content_to_blank_block() {
        let text = "<!-- BEGIN: zaz -->\nold\n<!-- END: zaz -->\n";
        let out = splice("zaz", "", text).unwrap();
        assert_eq!(out, "<!-- BEGIN: zaz -->\n<!-- END: zaz -->\n");
    }

    #[test]
    fn splice_preserves_html_comments_outside_markers() {
        let text = "\
<!-- a stray comment -->
<!-- BEGIN: zaz -->
old
<!-- END: zaz -->
<!-- another stray -->
";
        let out = splice("zaz", "new", text).unwrap();
        assert!(out.contains("<!-- a stray comment -->"));
        assert!(out.contains("<!-- another stray -->"));
        assert!(out.contains("\nnew\n"));
    }
}
