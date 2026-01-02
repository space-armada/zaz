//! UI rendering.

use crate::app::{App, Focus, InputMode, TuiStyle};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

/// Draw the main UI.
pub fn draw(frame: &mut Frame, app: &App) {
    match app.style {
        TuiStyle::Full => draw_full_style(frame, app),
        TuiStyle::Minimal => draw_minimal_style(frame, app),
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

fn draw_full_style(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Use taller status bar (5 lines) on larger screens for multi-line layout
    let status_bar_height = if area.height >= 20 && area.width >= 60 {
        5
    } else {
        3
    };

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_bar_height)])
        .split(chunks[0]);

    draw_groups(frame, app, left_chunks[0]);
    draw_status_bar(frame, app, left_chunks[1]);
    draw_logs(frame, app, chunks[1]);
}

fn draw_minimal_style(frame: &mut Frame, app: &App) {
    // Placeholder: will be fully implemented in Phase 7.8
    // For now, just show a simple layout
    let area = frame.area();

    // Use taller status bar (5 lines) on larger screens for multi-line layout
    let status_bar_height = if area.height >= 20 && area.width >= 60 {
        5
    } else {
        3
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_bar_height)])
        .split(area);

    draw_logs(frame, app, chunks[0]);
    draw_status_bar(frame, app, chunks[1]);
}

fn draw_groups(frame: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.focus == Focus::Groups {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let items: Vec<ListItem> = app
        .state
        .groups
        .values()
        .enumerate()
        .map(|(i, group)| {
            let status_icon = match group.status {
                zaz_daemon::GroupStatus::Pending => "○",
                zaz_daemon::GroupStatus::Running => {
                    if app.blink_on() {
                        "●"
                    } else {
                        "○"
                    }
                }
                zaz_daemon::GroupStatus::Ready => "✓",
                zaz_daemon::GroupStatus::Failed => "✗",
            };

            let status_color = match group.status {
                zaz_daemon::GroupStatus::Pending => Color::White,
                zaz_daemon::GroupStatus::Running => Color::Yellow,
                zaz_daemon::GroupStatus::Ready => Color::Green,
                zaz_daemon::GroupStatus::Failed => Color::Red,
            };

            let style = if i == app.selected_group {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", status_icon),
                    Style::default().fg(status_color),
                ),
                Span::styled(&group.name, style),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Groups ")
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_widget(list, area);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let connection_status = if app.is_connected() {
        Span::styled("●", Style::default().fg(Color::Green))
    } else {
        Span::styled("○", Style::default().fg(Color::Red))
    };

    // Calculate time since last change
    let last_change_text = if let Some(last_change_ms) = app.state.last_change {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let elapsed_secs = now_ms.saturating_sub(last_change_ms) / 1000;
        if elapsed_secs < 60 {
            format!("{}s since last change", elapsed_secs)
        } else if elapsed_secs < 3600 {
            format!("{}m since last change", elapsed_secs / 60)
        } else {
            format!("{}h since last change", elapsed_secs / 3600)
        }
    } else {
        "no changes".to_string()
    };

    let follow_icon = if app.logs.is_following() {
        "✓"
    } else {
        "○"
    };

    let filter_status = if app.logs.has_filter() {
        format!(" [filter: {}]", app.logs.filter_pattern().unwrap_or(""))
    } else {
        String::new()
    };

    // Use multi-line layout for larger screens (height >= 5 gives us 2 content lines)
    let use_multi_line = area.height >= 5 && area.width >= 60;

    let lines = if let Some(ref msg) = app.status_message {
        vec![Line::from(vec![
            Span::raw(" "),
            connection_status,
            Span::raw(" "),
            Span::raw(msg.clone()),
        ])]
    } else if use_multi_line {
        // Multi-line: detailed status
        vec![
            Line::from(vec![
                Span::raw(" "),
                connection_status.clone(),
                Span::raw(" Loaded "),
                Span::styled(
                    &app.config_name,
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" │ Follow "),
                Span::styled(
                    follow_icon,
                    if app.logs.is_following() {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(" │ "),
                Span::raw(&last_change_text),
                Span::raw(&filter_status),
            ]),
            Line::from(vec![
                Span::raw(" "),
                Span::styled("[f]", Style::default().fg(Color::Cyan)),
                Span::raw("ilter "),
                Span::styled("[/]", Style::default().fg(Color::Cyan)),
                Span::raw("search "),
                Span::styled("[q]", Style::default().fg(Color::Cyan)),
                Span::raw("uit "),
                Span::styled("[r]", Style::default().fg(Color::Cyan)),
                Span::raw("estart "),
                Span::styled("[?]", Style::default().fg(Color::Cyan)),
                Span::raw("help"),
            ]),
        ]
    } else {
        // Compact: minimal info
        vec![Line::from(vec![
            Span::raw(" "),
            connection_status,
            Span::raw(" "),
            Span::styled("[?]", Style::default().fg(Color::Cyan)),
            Span::raw(" help"),
        ])]
    };

    let paragraph =
        Paragraph::new(lines).block(Block::default().title(" Status ").borders(Borders::ALL));

    frame.render_widget(paragraph, area);
}

fn draw_logs(frame: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.focus == Focus::Logs {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    // Get combined logs from all processes
    let combined = app.logs.all_logs_combined();

    // Calculate visible range
    let visible_height = area.height.saturating_sub(2) as usize; // Account for borders
    let total_lines = combined.len();

    // Handle scroll position
    let scroll_offset = if app.logs.is_following() {
        // Auto-scroll to bottom
        total_lines.saturating_sub(visible_height)
    } else {
        app.log_scroll
            .min(total_lines.saturating_sub(visible_height))
    };

    let items: Vec<ListItem> = combined
        .iter()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(process, _idx, log)| {
            let is_match = app.logs.is_search_match(&log.content);
            let is_daemon_log = log.source == crate::daemon::LogSource::Daemon;

            let line_style = if is_match {
                Style::default().bg(Color::Yellow).fg(Color::Black)
            } else if is_daemon_log {
                Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };

            // Add [zaz] prefix for daemon logs
            let content = if is_daemon_log {
                format!("[zaz] {}", log.content)
            } else {
                log.content.clone()
            };

            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("[{}] ", process),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(content, line_style),
            ]))
        })
        .collect();

    let title = if total_lines > 0 {
        format!(
            " Logs ({}-{}/{}) ",
            scroll_offset + 1,
            (scroll_offset + items.len()).min(total_lines),
            total_lines
        )
    } else {
        " Logs (empty) ".to_string()
    };

    let list = List::new(items).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border_style),
    );

    frame.render_widget(list, area);
}

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
