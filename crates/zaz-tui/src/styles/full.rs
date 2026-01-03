//! Full style implementation.
//!
//! The full style shows a split-pane layout with:
//! - Left: Group tree with collapsible tasks/daemons
//! - Right: Combined log view with process prefixes
//! - Bottom: Status bar
//!
//! # Navigation
//!
//! - j/k or arrows: Move selection in groups tree or scroll logs
//! - Tab: Switch focus between groups and logs
//! - g/G: Jump to top/bottom of logs
//! - PgUp/PgDn: Scroll logs by page

use super::{KeyResult, PaneLayout, SelectedProcess, StyleRenderer};
use crate::app::{App, Focus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

/// Full style renderer with group tree and logs pane.
pub struct FullStyle;

impl StyleRenderer for FullStyle {
    fn draw(&self, frame: &mut Frame, app: &mut App) {
        let area = frame.area();

        // Status bar height
        let status_bar_height = if area.height >= 20 && area.width >= 60 {
            5
        } else {
            3
        };

        // First split: main content + status bar
        let vertical_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(status_bar_height)])
            .split(area);

        // Calculate the width needed for the groups pane based on content
        let groups_width = self.calculate_groups_width(app);
        let left_width = (groups_width + 4).min(area.width / 2).max(20);

        // Split main content: groups on left + logs on right
        let horizontal_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(left_width), Constraint::Min(0)])
            .split(vertical_chunks[0]);

        // Update visible height for scroll calculations
        app.log_visible_height = horizontal_chunks[1].height.saturating_sub(2) as usize;

        self.draw_groups(frame, app, horizontal_chunks[0]);
        self.draw_logs(frame, app, horizontal_chunks[1]);
        self.draw_status_bar(frame, app, vertical_chunks[1]);
    }

    fn handle_key(&self, app: &mut App, key: KeyEvent) -> KeyResult {
        // Handle Ctrl+key combinations first
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    // Half page down
                    let half_page = app.log_visible_height / 2;
                    let total = app.logs.all_logs_combined().len();
                    let max_scroll = total.saturating_sub(app.log_visible_height);

                    // Sync scroll position from follow mode before disabling
                    if app.logs.is_following() {
                        app.log_scroll = max_scroll;
                    }
                    app.logs.disable_follow();

                    app.log_scroll = (app.log_scroll + half_page).min(max_scroll);
                    return KeyResult::Handled;
                }
                KeyCode::Char('u') => {
                    // Half page up
                    let half_page = app.log_visible_height / 2;
                    let total = app.logs.all_logs_combined().len();
                    let max_scroll = total.saturating_sub(app.log_visible_height);

                    // Sync scroll position from follow mode before disabling
                    if app.logs.is_following() {
                        app.log_scroll = max_scroll;
                    }
                    app.logs.disable_follow();

                    app.log_scroll = app.log_scroll.saturating_sub(half_page);
                    return KeyResult::Handled;
                }
                _ => {}
            }
        }

        match key.code {
            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                self.navigate_down(app);
                KeyResult::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.navigate_up(app);
                KeyResult::Handled
            }
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::Groups => Focus::Logs,
                    Focus::Logs => Focus::Groups,
                    _ => Focus::Groups,
                };
                KeyResult::Handled
            }

            // Log scrolling
            KeyCode::Char('g') => {
                app.log_scroll = 0;
                app.logs.disable_follow();
                KeyResult::Handled
            }
            KeyCode::Char('G') => {
                let total = app.logs.all_logs_combined().len();
                app.log_scroll = total.saturating_sub(app.log_visible_height);
                app.logs.enable_follow();
                KeyResult::Handled
            }
            KeyCode::PageUp => {
                let total = app.logs.all_logs_combined().len();
                let max_scroll = total.saturating_sub(app.log_visible_height);

                // Sync scroll position from follow mode before disabling
                if app.logs.is_following() {
                    app.log_scroll = max_scroll;
                }
                app.logs.disable_follow();

                app.log_scroll = app.log_scroll.saturating_sub(app.log_visible_height);
                KeyResult::Handled
            }
            KeyCode::PageDown => {
                let total = app.logs.all_logs_combined().len();
                let max_scroll = total.saturating_sub(app.log_visible_height);

                // Sync scroll position from follow mode before disabling
                if app.logs.is_following() {
                    app.log_scroll = max_scroll;
                }
                app.logs.disable_follow();

                app.log_scroll = (app.log_scroll + app.log_visible_height).min(max_scroll);
                KeyResult::Handled
            }

            // Actions
            KeyCode::Char('r') => {
                if let Some(selected) = self.get_selected_process(app) {
                    KeyResult::Restart(selected)
                } else {
                    KeyResult::Handled
                }
            }
            KeyCode::Char('R') => KeyResult::RestartAll,
            KeyCode::Char('c') => {
                app.logs.clear_all();
                app.log_scroll = 0;
                KeyResult::SetStatus("Logs cleared".to_string())
            }

            // Follow mode
            KeyCode::Char('F') => {
                app.logs.toggle_follow();
                let status = if app.logs.is_following() {
                    "Follow mode ON"
                } else {
                    "Follow mode OFF"
                };
                KeyResult::SetStatus(status.to_string())
            }

            // Timestamp toggle
            KeyCode::Char('t') => {
                app.show_full_timestamp = !app.show_full_timestamp;
                let status = if app.show_full_timestamp {
                    "Full timestamps"
                } else {
                    "Compact timestamps"
                };
                KeyResult::SetStatus(status.to_string())
            }

            _ => KeyResult::NotHandled,
        }
    }

    fn name(&self) -> &'static str {
        "Full"
    }

    fn calculate_layout(&self, area: Rect, _task_count: usize) -> Vec<PaneLayout> {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        vec![
            PaneLayout {
                area: chunks[0],
                process: None,
                focused: false,
            },
            PaneLayout {
                area: chunks[1],
                process: None,
                focused: false,
            },
        ]
    }

    fn get_selected_process(&self, app: &App) -> Option<SelectedProcess> {
        let mut idx = 0;
        for group in app.state.groups.values() {
            // Check if group header is selected
            if app.selected_item == idx {
                return Some(SelectedProcess {
                    group: group.name.clone(),
                    process: group.name.clone(),
                    is_group: true,
                });
            }
            idx += 1;

            // Check tasks
            for task in &group.tasks {
                if app.selected_item == idx {
                    return Some(SelectedProcess {
                        group: group.name.clone(),
                        process: task.name.clone(),
                        is_group: false,
                    });
                }
                idx += 1;
            }

            // Check daemons
            for daemon in &group.daemons {
                if app.selected_item == idx {
                    return Some(SelectedProcess {
                        group: group.name.clone(),
                        process: daemon.name.clone(),
                        is_group: false,
                    });
                }
                idx += 1;
            }
        }
        None
    }

    fn on_activate(&self, app: &mut App) {
        app.focus = Focus::Groups;
    }

    fn log_dimensions(&self, app: &App) -> (usize, usize) {
        let combined = app.logs.all_logs_combined();
        (app.log_visible_height, combined.len())
    }
}

impl FullStyle {
    /// Calculate the width needed for the groups pane content.
    fn calculate_groups_width(&self, app: &App) -> u16 {
        let mut max_width: usize = 0;

        for group in app.state.groups.values() {
            // Group line: " ● group_name (status)"
            let group_width = 4 + group.name.len() + 10;
            max_width = max_width.max(group_width);

            // Task/daemon lines: "   ├─ [✓] name (suffix)"
            for task in &group.tasks {
                let suffix_len = task
                    .duration_ms
                    .map(|d| if d >= 1000 { 8 } else { 7 })
                    .unwrap_or(0);
                let task_width = 10 + task.name.len() + suffix_len;
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

    fn navigate_down(&self, app: &mut App) {
        match app.focus {
            Focus::Groups => {
                let total = self.groups_item_count(app);
                if total > 0 {
                    app.selected_item = (app.selected_item + 1) % total;
                }
            }
            Focus::Logs => {
                let total = app.logs.all_logs_combined().len();
                let max_scroll = total.saturating_sub(app.log_visible_height);

                // Sync scroll position from follow mode before disabling
                if app.logs.is_following() {
                    app.log_scroll = max_scroll;
                }
                app.logs.disable_follow();

                if app.log_scroll < max_scroll {
                    app.log_scroll += 1;
                }
            }
            _ => {}
        }
    }

    fn navigate_up(&self, app: &mut App) {
        match app.focus {
            Focus::Groups => {
                let total = self.groups_item_count(app);
                if total > 0 {
                    app.selected_item = app
                        .selected_item
                        .checked_sub(1)
                        .unwrap_or(total.saturating_sub(1));
                }
            }
            Focus::Logs => {
                // Sync scroll position from follow mode before disabling
                if app.logs.is_following() {
                    let total = app.logs.all_logs_combined().len();
                    let max_scroll = total.saturating_sub(app.log_visible_height);
                    app.log_scroll = max_scroll;
                }
                app.logs.disable_follow();

                if app.log_scroll > 0 {
                    app.log_scroll -= 1;
                }
            }
            _ => {}
        }
    }

    fn groups_item_count(&self, app: &App) -> usize {
        app.state
            .groups
            .values()
            .map(|g| 1 + g.tasks.len() + g.daemons.len())
            .sum()
    }

    fn draw_groups(&self, frame: &mut Frame, app: &App, area: Rect) {
        let border_style = if app.focus == Focus::Groups {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let mut items: Vec<ListItem> = Vec::new();
        let mut flat_idx: usize = 0;

        for group in app.state.groups.values() {
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

            // Tasks and daemons
            let total_children = group.tasks.len() + group.daemons.len();
            let mut child_idx = 0;

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

    fn draw_logs(&self, frame: &mut Frame, app: &App, area: Rect) {
        use crate::logs::timestamp_to_day;

        let border_style = if app.focus == Focus::Logs {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let combined = app.logs.all_logs_combined();
        let visible_height = area.height.saturating_sub(2) as usize;
        let total_lines = combined.len();

        let scroll_offset = if app.logs.is_following() {
            total_lines.saturating_sub(visible_height)
        } else {
            app.log_scroll
                .min(total_lines.saturating_sub(visible_height))
        };

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

                let content = if is_daemon_log {
                    format!("[zaz] {}", log.content)
                } else {
                    log.content.clone()
                };

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

    fn draw_status_bar(&self, frame: &mut Frame, app: &App, area: Rect) {
        let connection_status = if app.is_connected() {
            Span::styled("●", Style::default().fg(Color::Green))
        } else {
            Span::styled("○", Style::default().fg(Color::Red))
        };

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

        let use_multi_line = area.height >= 5 && area.width >= 60;

        let mut lines = if use_multi_line {
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
            vec![Line::from(vec![
                Span::raw(" "),
                connection_status,
                Span::raw(" "),
                Span::styled("[?]", Style::default().fg(Color::Cyan)),
                Span::raw(" help"),
            ])]
        };

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_style_name() {
        let style = FullStyle;
        assert_eq!(style.name(), "Full");
    }

    #[test]
    fn test_full_style_layout() {
        let style = FullStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 4);

        assert_eq!(layouts.len(), 2);
        assert!(layouts[0].area.width < 50);
        assert!(layouts[1].area.width > 50);
    }
}
