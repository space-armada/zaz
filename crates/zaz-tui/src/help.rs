//! Help overlay for the TUI.
//!
//! Displays keyboard shortcuts and usage information in a modal overlay.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

/// Help text sections.
const HELP_SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("j/k, ↓/↑", "Move down/up"),
            ("h/l, ←/→", "Move left/right"),
            ("Tab", "Switch focus/pane"),
            ("g/G", "Go to top/bottom of logs"),
            ("PgUp/PgDn", "Scroll logs by page"),
        ],
    ),
    (
        "Actions",
        &[
            ("r", "Restart selected group"),
            ("R", "Restart all groups"),
            ("c", "Clear logs"),
            ("F", "Toggle follow mode"),
        ],
    ),
    (
        "Search & Filter",
        &[
            ("/", "Start search"),
            ("f", "Start filter"),
            ("n/N", "Next/previous match"),
            ("Esc", "Clear search/filter"),
        ],
    ),
    (
        "Style",
        &[
            ("F1", "Switch to Full style"),
            ("F2", "Switch to Minimal style"),
            ("[/]", "Previous/next page (Minimal)"),
        ],
    ),
    (
        "General",
        &[
            ("q", "Quit"),
            ("?", "Toggle this help"),
        ],
    ),
];

/// Draw the help overlay.
pub fn draw_help(frame: &mut Frame) {
    let area = frame.area();

    // Calculate overlay size (80% of screen, max 80x40)
    let overlay_width = (area.width * 80 / 100).min(80);
    let overlay_height = (area.height * 80 / 100).min(40);

    let overlay_area = centered_rect(overlay_width, overlay_height, area);

    // Clear the background
    frame.render_widget(Clear, overlay_area);

    // Build help content
    let mut lines: Vec<Line> = vec![];

    // Title
    lines.push(Line::from(vec![
        Span::styled(
            "Keyboard Shortcuts",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    for (section_title, shortcuts) in HELP_SECTIONS {
        // Section header
        lines.push(Line::from(vec![Span::styled(
            format!("  {}", section_title),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));

        for (key, description) in *shortcuts {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {:12}", key),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(*description),
            ]));
        }

        lines.push(Line::from(""));
    }

    // Footer
    lines.push(Line::from(vec![Span::styled(
        "Press ? or Esc to close",
        Style::default().fg(Color::DarkGray),
    )]));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Help ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, overlay_area);
}

/// Create a centered rect within the given area.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;

    Rect {
        x: area.x + x,
        y: area.y + y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_centered_rect() {
        let area = Rect::new(0, 0, 100, 50);
        let centered = centered_rect(40, 20, area);

        assert_eq!(centered.x, 30);
        assert_eq!(centered.y, 15);
        assert_eq!(centered.width, 40);
        assert_eq!(centered.height, 20);
    }

    #[test]
    fn test_centered_rect_too_large() {
        let area = Rect::new(0, 0, 40, 20);
        let centered = centered_rect(80, 40, area);

        // Should be clamped to area size
        assert_eq!(centered.width, 40);
        assert_eq!(centered.height, 20);
    }
}
