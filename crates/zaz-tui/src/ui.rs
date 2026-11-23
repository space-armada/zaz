//! UI rendering.
//!
//! This module provides the main UI drawing function that delegates to
//! style-specific renderers. It also handles overlays like input prompts
//! and help screens.

use crate::app::{App, InputMode};
use crate::styles::get_renderer;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use zaz_daemon::{DaemonState, DaemonStatus};

/// Draw the main UI.
///
/// This function:
/// 1. Delegates to the active style's renderer for the main UI
/// 2. Draws input overlays (filter/search) if in input mode
/// 3. Draws the help overlay if showing
pub fn draw(frame: &mut Frame, app: &mut App) {
    // Delegate to the active style's renderer
    let renderer = get_renderer(app.style);
    renderer.draw(frame, app);

    // The default state reads as starting, so only a daemon that has actually reported gets one.
    if app.is_connected() {
        if let Some(banner) = daemon_banner(&app.state) {
            draw_daemon_banner(frame, &banner);
        }
    }

    // Draw input overlay if in input mode
    match app.input_mode {
        InputMode::Filter => draw_input_overlay(frame, "Filter: ", &app.filter_input),
        InputMode::Search => draw_input_overlay(frame, "Search: ", &app.search_input),
        InputMode::QuitPrompt => draw_quit_prompt(frame),
        InputMode::Normal => {}
    }

    // Draw help overlay if showing
    if app.show_help {
        crate::help::draw_help(frame);
    }
}

/// A line the TUI shows while the daemon has not finished loading its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonBanner {
    pub text: String,
    pub is_error: bool,
}

/// The banner for a daemon that is still starting, or whose first load failed.
///
/// A daemon that is running, or one that is shutting down, needs none.
pub fn daemon_banner(state: &DaemonState) -> Option<DaemonBanner> {
    if let Some(error) = &state.load_error {
        let first_line = error.message.lines().next().unwrap_or_default();

        return Some(DaemonBanner {
            text: format!(
                "Config failed to load [{}]: {} Fix it and run `zaz reload`.",
                error.code, first_line
            ),
            is_error: true,
        });
    }

    if state.status == DaemonStatus::Starting {
        return Some(DaemonBanner {
            text: "Daemon is loading its configuration...".to_string(),
            is_error: false,
        });
    }

    None
}

/// Draw the daemon banner on the bottom row.
fn draw_daemon_banner(frame: &mut Frame, banner: &DaemonBanner) {
    let area = frame.area();
    if area.height == 0 {
        return;
    }

    let row = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let color = if banner.is_error {
        Color::Red
    } else {
        Color::Yellow
    };

    frame.render_widget(Clear, row);
    frame.render_widget(
        Paragraph::new(Span::styled(
            banner.text.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        row,
    );
}

/// Draw the input overlay for filter/search mode.
fn draw_input_overlay(frame: &mut Frame, prompt: &str, input: &str) {
    let area = frame.area();
    let input_area = Rect {
        x: area.x + 1,
        y: area.height.saturating_sub(3),
        width: area.width.saturating_sub(2),
        height: 3,
    };

    let paragraph = Paragraph::new(format!("{}{}_", prompt, input)).block(
        Block::default()
            .title(" Input (Enter=confirm, Esc=cancel) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(paragraph, input_area);
}

/// Draw the quit confirmation prompt.
fn draw_quit_prompt(frame: &mut Frame) {
    let area = frame.area();
    let popup_width = 50;
    let popup_height = 6;
    let popup_area = Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width.min(area.width),
        height: popup_height.min(area.height),
    };

    // Clear the background
    frame.render_widget(Clear, popup_area);

    // Draw background block
    let background = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Quit? ");
    frame.render_widget(background, popup_area);

    // Draw text inside
    let text_area = Rect {
        x: popup_area.x + 2,
        y: popup_area.y + 2,
        width: popup_area.width.saturating_sub(4),
        height: popup_area.height.saturating_sub(3),
    };

    let paragraph = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "[d]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" detach (keep daemon running)"),
        ]),
        Line::from(vec![
            Span::styled(
                "[q]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" quit (stop daemon)"),
        ]),
    ]);

    frame.render_widget(paragraph, text_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaz_daemon::LoadError;

    #[test]
    fn a_starting_daemon_gets_a_progress_banner() {
        let banner = daemon_banner(&DaemonState::default()).expect("starting shows a banner");

        assert!(!banner.is_error);
        assert!(banner.text.contains("loading"));
    }

    #[test]
    fn a_failed_first_load_gets_an_error_banner_with_the_code_and_the_next_step() {
        let state = DaemonState {
            load_error: Some(LoadError::new(
                "env_file_missing",
                "env_file not found [env_file_missing]\nsecond line",
            )),
            ..Default::default()
        };

        let banner = daemon_banner(&state).expect("a load error shows a banner");

        assert!(banner.is_error);
        assert!(banner.text.contains("env_file_missing"));
        assert!(banner.text.contains("zaz reload"));
        assert!(!banner.text.contains("second line"));
    }

    #[test]
    fn a_running_daemon_needs_no_banner() {
        let state = DaemonState {
            status: DaemonStatus::Running,
            ..Default::default()
        };

        assert_eq!(daemon_banner(&state), None);
    }
}
