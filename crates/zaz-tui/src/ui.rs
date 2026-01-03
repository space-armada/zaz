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
