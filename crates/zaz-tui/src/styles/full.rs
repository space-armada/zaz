//! Full style implementation.
//!
//! The full style shows a split-pane layout with:
//! - Left: Group tree with collapsible tasks/daemons
//! - Right: Combined log view with process prefixes
//! - Bottom: Status bar

use super::{PaneLayout, StyleRenderer};
use crate::app::{App, Focus};
use crossterm::event::KeyCode;
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
    fn draw(&self, frame: &mut Frame, app: &App) {
        let area = frame.area();
        let _layouts = self.calculate_layout(area, app.task_count());

        // Main horizontal split: groups (40%) | logs (60%)
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        // Left side: groups + status
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(chunks[0]);

        self.draw_groups(frame, app, left_chunks[0]);
        self.draw_status_bar(frame, app, left_chunks[1]);
        self.draw_logs(frame, app, chunks[1]);
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

    fn handle_navigation(&self, app: &mut App, key: KeyCode) {
        let total_items: usize = app
            .state
            .groups
            .values()
            .map(|g| 1 + g.tasks.len() + g.daemons.len())
            .sum();

        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                if total_items > 0 {
                    app.selected_item = (app.selected_item + 1) % total_items;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if total_items > 0 {
                    app.selected_item = app
                        .selected_item
                        .checked_sub(1)
                        .unwrap_or(total_items.saturating_sub(1));
                }
            }
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::Groups => Focus::Logs,
                    Focus::Logs => Focus::Groups,
                    _ => Focus::Groups,
                };
            }
            _ => {}
        }
    }

    fn name(&self) -> &'static str {
        "Full"
    }
}

impl FullStyle {
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

            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", status_icon), Style::default().fg(status_color)),
                Span::styled(&group.name, style),
            ])));

            // Add tasks and daemons (update flat_idx for each)
            for _task in &group.tasks {
                flat_idx += 1;
            }
            for _daemon in &group.daemons {
                flat_idx += 1;
            }
        }

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

    fn draw_status_bar(&self, frame: &mut Frame, app: &App, area: Rect) {
        let connection_status = if app.is_connected() {
            Span::styled("●", Style::default().fg(Color::Green))
        } else {
            Span::styled("○", Style::default().fg(Color::Red))
        };

        let filter_status = if app.logs.has_filter() {
            format!(" [filter: {}]", app.logs.filter_pattern().unwrap_or(""))
        } else {
            String::new()
        };

        let status = if let Some(ref msg) = app.status_message {
            msg.clone()
        } else {
            format!(
                " F1:Full* | {} | [q]uit [r]estart [?]help{}",
                if app.logs.is_following() {
                    "Follow:ON"
                } else {
                    "Follow:OFF"
                },
                filter_status
            )
        };

        let line = Line::from(vec![
            Span::raw(" "),
            connection_status,
            Span::raw(" "),
            Span::raw(status),
        ]);

        let paragraph =
            Paragraph::new(line).block(Block::default().title(" Status ").borders(Borders::ALL));

        frame.render_widget(paragraph, area);
    }

    fn draw_logs(&self, frame: &mut Frame, app: &App, area: Rect) {
        use crate::logs::timestamp_to_day;

        let border_style = if app.focus == Focus::Logs {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        // Get combined logs from all processes
        let combined = app.logs.all_logs_combined();

        // Calculate visible range
        let visible_height = area.height.saturating_sub(2) as usize;
        let total_lines = combined.len();

        // Handle scroll position
        let scroll_offset = if app.logs.is_following() {
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
                    Style::default().bg(Color::Yellow).fg(Color::Black)
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
        // Left pane should be ~40%
        assert!(layouts[0].area.width < 50);
        // Right pane should be ~60%
        assert!(layouts[1].area.width > 50);
    }
}
