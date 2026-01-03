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
pub fn draw(frame: &mut Frame, app: &mut App) {
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

fn draw_full_style(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Status bar height: 5 lines for multi-line layout, 3 for compact
    // Avoid resizing when transient message comes in to avoid resizes
    let status_bar_height = if area.height >= 20 && area.width >= 60 {
        5
    } else {
        3
    };

    // First split: main content + status bar at bottom (full width)
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_bar_height)])
        .split(area);

    // Calculate the width needed for the groups pane based on content
    let groups_width = calculate_groups_width(app);
    // Add 4 for borders and padding, cap at 50% of screen
    let left_width = (groups_width + 4).min(area.width / 2).max(20);

    // Then split main content: groups on left + logs on right
    let horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_width), Constraint::Min(0)])
        .split(vertical_chunks[0]);

    draw_groups(frame, app, horizontal_chunks[0]);
    draw_logs(frame, app, horizontal_chunks[1]);
    draw_status_bar(frame, app, vertical_chunks[1]);
}

/// Calculate the width needed for the groups pane content.
fn calculate_groups_width(app: &App) -> u16 {
    let mut max_width: usize = 0;

    for group in app.state.groups.values() {
        // Group line: " ● group_name (status)"
        let group_width = 4 + group.name.len() + 10; // icon + name + " (running)"
        max_width = max_width.max(group_width);

        // Task/daemon lines: "   ├─ [✓] name (suffix)"
        for task in &group.tasks {
            let suffix_len = task
                .duration_ms
                .map(|d| if d >= 1000 { 8 } else { 7 })
                .unwrap_or(0);
            let task_width = 10 + task.name.len() + suffix_len; // "   ├─ [✓] " + name + suffix
            max_width = max_width.max(task_width);
        }

        for daemon in &group.daemons {
            let suffix_len = daemon
                .pid
                .map(|p| format!(" (pid {})", p).len())
                .unwrap_or(0);
            let daemon_width = 10 + daemon.name.len() + suffix_len;
            max_width = max_width.max(daemon_width);
        }
    }

    max_width as u16
}

fn draw_minimal_style(frame: &mut Frame, app: &App) {
    use crate::styles::{MinimalStyle, StyleRenderer};
    let style = MinimalStyle;
    style.draw(frame, app);
}

fn draw_groups(frame: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.focus == Focus::Groups {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let mut items: Vec<ListItem> = Vec::new();
    let mut flat_idx: usize = 0;

    for group in app.state.groups.values() {
        // Group header
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

        let is_selected = flat_idx == app.selected_item;
        flat_idx += 1;

        let style = if is_selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        // Format group status text
        let group_status_text = match group.status {
            zaz_daemon::GroupStatus::Pending => "pending",
            zaz_daemon::GroupStatus::Running => "running",
            zaz_daemon::GroupStatus::Ready => "ready",
            zaz_daemon::GroupStatus::Failed => "failed",
        };

        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {} ", status_icon),
                Style::default().fg(status_color),
            ),
            Span::styled(&group.name, style),
            Span::styled(
                format!(" ({})", group_status_text),
                Style::default().fg(Color::DarkGray),
            ),
        ])));

        // Collect all children (tasks + daemons) for tree rendering
        let total_children = group.tasks.len() + group.daemons.len();
        let mut child_idx = 0;

        // Tasks under the group
        for task in &group.tasks {
            child_idx += 1;
            let is_last = child_idx == total_children;
            let tree_branch = if is_last { "└─" } else { "├─" };

            let is_selected = flat_idx == app.selected_item;
            flat_idx += 1;

            let task_icon = match task.status {
                zaz_daemon::ProcessStatus::Pending => "○",
                zaz_daemon::ProcessStatus::Running => {
                    if app.blink_on() {
                        "●"
                    } else {
                        "○"
                    }
                }
                zaz_daemon::ProcessStatus::Success => "✓",
                zaz_daemon::ProcessStatus::Failed => "✗",
                zaz_daemon::ProcessStatus::Backoff => "⟳",
            };

            let task_color = match task.status {
                zaz_daemon::ProcessStatus::Pending => Color::DarkGray,
                zaz_daemon::ProcessStatus::Running => Color::Yellow,
                zaz_daemon::ProcessStatus::Success => Color::Green,
                zaz_daemon::ProcessStatus::Failed => Color::Red,
                zaz_daemon::ProcessStatus::Backoff => Color::Yellow,
            };

            // Format duration or status suffix
            let suffix = match task.status {
                zaz_daemon::ProcessStatus::Success | zaz_daemon::ProcessStatus::Failed => task
                    .duration_ms
                    .map(|d| {
                        if d >= 1000 {
                            format!(" ({:.1}s)", d as f64 / 1000.0)
                        } else {
                            format!(" ({}ms)", d)
                        }
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };

            let name_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("   {} ", tree_branch),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(format!("[{}] ", task_icon), Style::default().fg(task_color)),
                Span::styled(&task.name, name_style),
                Span::styled(suffix, Style::default().fg(Color::DarkGray)),
            ])));
        }

        // Daemons under the group
        for daemon in &group.daemons {
            child_idx += 1;
            let is_last = child_idx == total_children;
            let tree_branch = if is_last { "└─" } else { "├─" };

            let is_selected = flat_idx == app.selected_item;
            flat_idx += 1;

            let daemon_icon = match daemon.status {
                zaz_daemon::ProcessStatus::Pending => "○",
                zaz_daemon::ProcessStatus::Running => {
                    if app.blink_on() {
                        "●"
                    } else {
                        "○"
                    }
                }
                zaz_daemon::ProcessStatus::Success => "✓",
                zaz_daemon::ProcessStatus::Failed => "✗",
                zaz_daemon::ProcessStatus::Backoff => "⟳",
            };

            let daemon_color = match daemon.status {
                zaz_daemon::ProcessStatus::Pending => Color::DarkGray,
                zaz_daemon::ProcessStatus::Running => Color::Yellow,
                zaz_daemon::ProcessStatus::Success => Color::Green,
                zaz_daemon::ProcessStatus::Failed => Color::Red,
                zaz_daemon::ProcessStatus::Backoff => Color::Yellow,
            };

            // Format pid suffix for running daemons
            let suffix = match daemon.status {
                zaz_daemon::ProcessStatus::Running => daemon
                    .pid
                    .map(|p| format!(" (pid {})", p))
                    .unwrap_or_default(),
                _ => String::new(),
            };

            let name_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("   {} ", tree_branch),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("[{}] ", daemon_icon),
                    Style::default().fg(daemon_color),
                ),
                Span::styled(&daemon.name, name_style),
                Span::styled(suffix, Style::default().fg(Color::DarkGray)),
            ])));
        }
    }

    let title = if app.focus == Focus::Groups {
        " Groups* "
    } else {
        " Groups "
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
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

    let mut lines = if use_multi_line {
        // Multi-line: detailed status
        let daemon_indicator = if app.started_daemon { " (daemon)" } else { "" };
        vec![
            Line::from(vec![
                Span::raw(" "),
                connection_status.clone(),
                Span::raw(" Loaded "),
                Span::styled(
                    &app.config_name,
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(daemon_indicator, Style::default().fg(Color::Cyan)),
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

    // Add transient message line if present and not expired
    if let Some(msg) = app.active_transient_message() {
        let style = if msg.is_error {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Green)
        };
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("→ ", style),
            Span::styled(&msg.text, style),
        ]));
    }

    let paragraph =
        Paragraph::new(lines).block(Block::default().title(" Status ").borders(Borders::ALL));

    frame.render_widget(paragraph, area);
}

fn draw_logs(frame: &mut Frame, app: &mut App, area: Rect) {
    use crate::logs::timestamp_to_day;

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

    // Update app's visible height for scroll calculations
    app.log_visible_height = visible_height;

    // Handle scroll position
    let scroll_offset = if app.logs.is_following() {
        // Auto-scroll to bottom
        total_lines.saturating_sub(visible_height)
    } else {
        app.log_scroll
            .min(total_lines.saturating_sub(visible_height))
    };

    // Get reference day from first log (for day offset calculation)
    let reference_day = combined
        .first()
        .map(|(_, _, log)| timestamp_to_day(log.timestamp))
        .unwrap_or(0);

    let items: Vec<ListItem> = combined
        .iter()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(process, _idx, log)| {
            let is_match = app.logs.is_search_match(&log.content);
            let is_daemon_log = log.source == crate::daemon::LogSource::Daemon;

            let line_style = if is_match {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if is_daemon_log {
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };

            // Add [zaz] prefix for daemon logs
            let content = if is_daemon_log {
                format!("[zaz] {}", log.content)
            } else {
                log.content.clone()
            };

            // Format timestamp
            let timestamp = log.format_timestamp(reference_day, app.show_full_timestamp);

            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("[{}] ", process),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(content, line_style),
            ]))
        })
        .collect();

    let focus_indicator = if app.focus == Focus::Logs { "*" } else { "" };
    let title = if total_lines > 0 {
        format!(
            " Logs{} ({}-{}/{}) ",
            focus_indicator,
            scroll_offset + 1,
            (scroll_offset + items.len()).min(total_lines),
            total_lines
        )
    } else {
        format!(" Logs{} (empty) ", focus_indicator)
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
