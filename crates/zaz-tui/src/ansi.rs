//! ANSI color code parsing for log display.
//!
//! Converts ANSI escape sequences in log content to ratatui styled spans,
//! enabling proper color rendering in the TUI.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use std::sync::LazyLock;

/// Regex to match ANSI color/style codes (SGR sequences).
/// Matches: ESC[<params>m where params are semicolon-separated numbers.
static ANSI_SGR_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[([0-9;]*)m").unwrap());

/// Parse a string with ANSI codes into styled spans.
///
/// This function converts ANSI SGR (Select Graphic Rendition) escape sequences
/// into ratatui styles. Supported codes:
/// - Reset: 0
/// - Bold: 1, Dim: 2, Italic: 3, Underline: 4
/// - Foreground colors: 30-37 (standard), 90-97 (bright)
/// - Background colors: 40-47 (standard), 100-107 (bright)
/// - Default foreground: 39, Default background: 49
///
/// # Example
///
/// ```ignore
/// let line = parse_ansi("\x1b[31mError\x1b[0m: something failed");
/// // Returns a Line with "Error" in red and ": something failed" in default style
/// ```
pub fn parse_ansi(input: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current_style = Style::default();
    let mut last_end = 0;

    for cap in ANSI_SGR_REGEX.captures_iter(input) {
        let full_match = cap.get(0).unwrap();
        let match_start = full_match.start();
        let match_end = full_match.end();

        // Add text before this escape sequence
        if match_start > last_end {
            let text = &input[last_end..match_start];
            if !text.is_empty() {
                spans.push(Span::styled(text.to_string(), current_style));
            }
        }

        // Parse the escape sequence and update style
        let codes = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        current_style = apply_ansi_codes(current_style, codes);

        last_end = match_end;
    }

    // Add remaining text after the last escape sequence
    if last_end < input.len() {
        let text = &input[last_end..];
        if !text.is_empty() {
            spans.push(Span::styled(text.to_string(), current_style));
        }
    }

    // If no ANSI codes found, return the input as-is
    if spans.is_empty() {
        Line::from(input.to_string())
    } else {
        Line::from(spans)
    }
}

/// Parse a string with ANSI codes, prepending spans before the parsed content.
///
/// This is useful for adding prefixes (timestamp, process name) before parsed log content.
pub fn parse_ansi_with_prefix(prefix_spans: Vec<Span<'static>>, content: &str) -> Line<'static> {
    let content_line = parse_ansi(content);
    let mut all_spans = prefix_spans;
    all_spans.extend(content_line.spans);
    Line::from(all_spans)
}

/// Apply ANSI SGR codes to a style.
fn apply_ansi_codes(style: Style, codes: &str) -> Style {
    // Empty codes string means reset (ESC[m is equivalent to ESC[0m)
    if codes.is_empty() {
        return Style::default();
    }

    let mut result = style;

    for code_str in codes.split(';') {
        let code: u8 = match code_str.parse() {
            Ok(c) => c,
            Err(_) => continue,
        };

        result = match code {
            // Reset
            0 => Style::default(),

            // Text attributes
            1 => result.add_modifier(Modifier::BOLD),
            2 => result.add_modifier(Modifier::DIM),
            3 => result.add_modifier(Modifier::ITALIC),
            4 => result.add_modifier(Modifier::UNDERLINED),
            7 => result.add_modifier(Modifier::REVERSED),
            9 => result.add_modifier(Modifier::CROSSED_OUT),

            // Remove attributes
            22 => result.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => result.remove_modifier(Modifier::ITALIC),
            24 => result.remove_modifier(Modifier::UNDERLINED),
            27 => result.remove_modifier(Modifier::REVERSED),
            29 => result.remove_modifier(Modifier::CROSSED_OUT),

            // Standard foreground colors (30-37)
            30 => result.fg(Color::Black),
            31 => result.fg(Color::Red),
            32 => result.fg(Color::Green),
            33 => result.fg(Color::Yellow),
            34 => result.fg(Color::Blue),
            35 => result.fg(Color::Magenta),
            36 => result.fg(Color::Cyan),
            37 => result.fg(Color::White),
            39 => result.fg(Color::Reset), // Default foreground

            // Standard background colors (40-47)
            40 => result.bg(Color::Black),
            41 => result.bg(Color::Red),
            42 => result.bg(Color::Green),
            43 => result.bg(Color::Yellow),
            44 => result.bg(Color::Blue),
            45 => result.bg(Color::Magenta),
            46 => result.bg(Color::Cyan),
            47 => result.bg(Color::White),
            49 => result.bg(Color::Reset), // Default background

            // Bright foreground colors (90-97)
            90 => result.fg(Color::DarkGray),
            91 => result.fg(Color::LightRed),
            92 => result.fg(Color::LightGreen),
            93 => result.fg(Color::LightYellow),
            94 => result.fg(Color::LightBlue),
            95 => result.fg(Color::LightMagenta),
            96 => result.fg(Color::LightCyan),
            97 => result.fg(Color::White),

            // Bright background colors (100-107)
            100 => result.bg(Color::DarkGray),
            101 => result.bg(Color::LightRed),
            102 => result.bg(Color::LightGreen),
            103 => result.bg(Color::LightYellow),
            104 => result.bg(Color::LightBlue),
            105 => result.bg(Color::LightMagenta),
            106 => result.bg(Color::LightCyan),
            107 => result.bg(Color::White),

            // Unhandled codes are ignored
            _ => result,
        };
    }

    result
}

/// Strip all ANSI escape codes from a string.
///
/// Useful for getting plain text content for matching/filtering.
pub fn strip_ansi(input: &str) -> String {
    ANSI_SGR_REGEX.replace_all(input, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_text() {
        let line = parse_ansi("hello world");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "hello world");
    }

    #[test]
    fn test_parse_simple_color() {
        let line = parse_ansi("\x1b[31mError\x1b[0m");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "Error");
        assert_eq!(line.spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn test_parse_color_with_text_after() {
        let line = parse_ansi("\x1b[31mError\x1b[0m: something failed");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content, "Error");
        assert_eq!(line.spans[0].style.fg, Some(Color::Red));
        assert_eq!(line.spans[1].content, ": something failed");
        assert_eq!(line.spans[1].style.fg, None); // Reset
    }

    #[test]
    fn test_parse_multiple_colors() {
        let line = parse_ansi("\x1b[32mOK\x1b[0m \x1b[33mWARN\x1b[0m \x1b[31mERR\x1b[0m");
        assert_eq!(line.spans.len(), 5); // OK, space, WARN, space, ERR
        assert_eq!(line.spans[0].style.fg, Some(Color::Green));
        assert_eq!(line.spans[2].style.fg, Some(Color::Yellow));
        assert_eq!(line.spans[4].style.fg, Some(Color::Red));
    }

    #[test]
    fn test_parse_bold() {
        let line = parse_ansi("\x1b[1mBold\x1b[0m");
        assert_eq!(line.spans[0].content, "Bold");
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_parse_combined_codes() {
        // Bold red: ESC[1;31m
        let line = parse_ansi("\x1b[1;31mBold Red\x1b[0m");
        assert_eq!(line.spans[0].content, "Bold Red");
        assert_eq!(line.spans[0].style.fg, Some(Color::Red));
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_parse_bright_colors() {
        let line = parse_ansi("\x1b[91mBright Red\x1b[0m");
        assert_eq!(line.spans[0].style.fg, Some(Color::LightRed));
    }

    #[test]
    fn test_parse_background_color() {
        let line = parse_ansi("\x1b[44mBlue BG\x1b[0m");
        assert_eq!(line.spans[0].style.bg, Some(Color::Blue));
    }

    #[test]
    fn test_parse_empty_reset() {
        // ESC[m is equivalent to ESC[0m (reset)
        let line = parse_ansi("\x1b[31mRed\x1b[mNormal");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].style.fg, Some(Color::Red));
        assert_eq!(line.spans[1].style.fg, None);
    }

    #[test]
    fn test_strip_ansi() {
        let input = "\x1b[31mError\x1b[0m: \x1b[1msomething\x1b[0m failed";
        let stripped = strip_ansi(input);
        assert_eq!(stripped, "Error: something failed");
    }

    #[test]
    fn test_parse_with_prefix() {
        let prefix = vec![
            Span::styled("12:00:00 ", Style::default().fg(Color::DarkGray)),
            Span::styled("[server] ", Style::default().fg(Color::DarkGray)),
        ];
        let line = parse_ansi_with_prefix(prefix, "\x1b[32mStarted\x1b[0m");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].content, "12:00:00 ");
        assert_eq!(line.spans[1].content, "[server] ");
        assert_eq!(line.spans[2].content, "Started");
        assert_eq!(line.spans[2].style.fg, Some(Color::Green));
    }

    #[test]
    fn test_dim_modifier() {
        let line = parse_ansi("\x1b[2mDim text\x1b[0m");
        assert!(line.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn test_underline() {
        let line = parse_ansi("\x1b[4mUnderlined\x1b[0m");
        assert!(line.spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_no_ansi_returns_single_span() {
        let line = parse_ansi("Plain text without any escape codes");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "Plain text without any escape codes");
    }
}
