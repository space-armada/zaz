//! Stripping terminal escape sequences from text.

use regex::Regex;
use std::sync::LazyLock;

// Alternatives are tried in order, so each introducer picks its own body before the generic
// two-byte form. An unterminated OSC or string sequence ends at the next ESC or BEL, and a
// truncated CSI ends where its bytes stop matching, so none of them swallows the rest of the text.
static ESCAPE_SEQUENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)\x1b(?:\][^\x07\x1b]*(?:\x07|\x1b\\)?|[PX^_][^\x1b]*(?:\x1b\\)?|\[[0-?]*[ -/]*[@-~]?|[ -/]*[0-~]?)",
    )
    .expect("the escape sequence pattern is valid")
});

/// Strip every escape sequence from a string.
///
/// This removes CSI sequences such as colors and cursor moves, OSC sequences such as window titles
/// and hyperlinks, device and string controls, and the remaining two-byte escapes. Text between
/// sequences is kept as written.
pub fn strip_ansi(input: &str) -> String {
    ESCAPE_SEQUENCE.replace_all(input, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_are_stripped() {
        let input = "\x1b[31mError\x1b[0m: \x1b[1msomething\x1b[0m failed";

        assert_eq!(strip_ansi(input), "Error: something failed");
    }

    #[test]
    fn cursor_and_erase_sequences_are_stripped() {
        assert_eq!(strip_ansi("a\x1b[2K\x1b[1Gb\x1b[?25lc"), "abc");
    }

    #[test]
    fn osc_sequences_end_at_bel_or_string_terminator() {
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
        assert_eq!(
            strip_ansi("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
    }

    #[test]
    fn string_controls_and_two_byte_escapes_are_stripped() {
        assert_eq!(strip_ansi("a\x1bPdata\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1b=b\x1bMc\x1b(0d"), "abcd");
    }

    #[test]
    fn truncated_sequences_do_not_swallow_the_text_after_them() {
        assert_eq!(strip_ansi("a\x1b[31"), "a");
        assert_eq!(strip_ansi("a\x1b"), "a");
        assert_eq!(strip_ansi("a\x1b]0;title"), "a");
    }

    #[test]
    fn text_without_escapes_is_unchanged() {
        assert_eq!(strip_ansi("plain [31m text\n"), "plain [31m text\n");
    }
}
