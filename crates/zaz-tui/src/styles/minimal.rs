//! Minimal style implementation.
//!
//! The minimal style shows one pane per task/daemon with adaptive layout:
//! - 1 task: Single full-screen pane
//! - 2 tasks: Horizontal or vertical split based on terminal aspect ratio
//! - 3-4 tasks: 2x2 grid
//! - 5-6 tasks: 3x2 or 2x3 grid based on terminal aspect ratio
//! - 7+ tasks: Paginated 2x3 grid (6 per page)

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

/// Minimal style renderer with one pane per task.
pub struct MinimalStyle;

impl StyleRenderer for MinimalStyle {
    fn draw(&self, frame: &mut Frame, app: &App) {
        let area = frame.area();

        // Reserve space for status bar
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        let main_area = chunks[0];
        let status_area = chunks[1];

        // Get all tasks/daemons
        let processes = self.get_processes(app);
        let task_count = processes.len();

        if task_count == 0 {
            self.draw_empty(frame, main_area);
        } else {
            let layouts = self.calculate_layout(main_area, task_count);

            // Calculate which tasks to show on current page
            let start_idx = app.current_page * 6;
            let end_idx = (start_idx + 6).min(task_count);
            let visible_processes: Vec<_> = processes
                .iter()
                .skip(start_idx)
                .take(end_idx - start_idx)
                .collect();

            for (i, (pane_layout, process)) in
                layouts.iter().zip(visible_processes.iter()).enumerate()
            {
                let global_idx = start_idx + i;
                let is_focused = app.selected_pane == global_idx;
                self.draw_pane(frame, app, pane_layout.area, process, is_focused);
            }
        }

        self.draw_status_bar(frame, app, status_area, task_count);
    }

    fn calculate_layout(&self, area: Rect, task_count: usize) -> Vec<PaneLayout> {
        let is_wide = (area.width as f32 / area.height as f32) > 2.0;
        let visible_count = task_count.min(6);

        let rects = match visible_count {
            0 => vec![area],
            1 => vec![area],
            2 => {
                if is_wide {
                    self.hsplit(area, 2)
                } else {
                    self.vsplit(area, 2)
                }
            }
            3 | 4 => self.grid(area, 2, 2),
            5 | 6 => {
                if is_wide {
                    self.grid(area, 3, 2) // 3 columns, 2 rows
                } else {
                    self.grid(area, 2, 3) // 2 columns, 3 rows
                }
            }
            _ => self.grid(area, 2, 3), // 2x3 paginated
        };

        rects
            .into_iter()
            .take(visible_count)
            .map(|r| PaneLayout {
                area: r,
                process: None,
                focused: false,
            })
            .collect()
    }

    fn handle_navigation(&self, app: &mut App, key: KeyCode) {
        let task_count = app.task_count();
        if task_count == 0 {
            return;
        }

        match key {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                app.selected_pane = (app.selected_pane + 1) % task_count;
                app.current_page = app.selected_pane / 6;
                app.focus = Focus::Pane(app.selected_pane);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.selected_pane = app
                    .selected_pane
                    .checked_sub(1)
                    .unwrap_or(task_count.saturating_sub(1));
                app.current_page = app.selected_pane / 6;
                app.focus = Focus::Pane(app.selected_pane);
            }
            KeyCode::Char('h') | KeyCode::Left => {
                // Move left in grid
                let cols = self.columns_for_count(task_count.min(6));
                if app.selected_pane % cols > 0 {
                    app.selected_pane -= 1;
                    app.focus = Focus::Pane(app.selected_pane);
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                // Move right in grid
                let cols = self.columns_for_count(task_count.min(6));
                let page_offset = app.current_page * 6;
                let local_idx = app.selected_pane - page_offset;
                if local_idx % cols < cols - 1 && app.selected_pane + 1 < task_count {
                    app.selected_pane += 1;
                    app.focus = Focus::Pane(app.selected_pane);
                }
            }
            KeyCode::Char('[') => {
                if app.current_page > 0 {
                    app.current_page -= 1;
                    app.selected_pane = app.current_page * 6;
                    app.focus = Focus::Pane(app.selected_pane);
                }
            }
            KeyCode::Char(']') => {
                let max_page = (task_count.saturating_sub(1)) / 6;
                if app.current_page < max_page {
                    app.current_page += 1;
                    app.selected_pane = app.current_page * 6;
                    app.focus = Focus::Pane(app.selected_pane);
                }
            }
            _ => {}
        }
    }

    fn name(&self) -> &'static str {
        "Minimal"
    }
}

/// Process info for rendering.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub name: String,
    pub group: String,
    pub status: ProcessStatus,
    pub kind: ProcessKind,
}

#[derive(Debug, Clone, Copy)]
pub enum ProcessStatus {
    Pending,
    Running,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy)]
pub enum ProcessKind {
    Task,
    Daemon,
}

impl MinimalStyle {
    /// Get all processes (tasks + daemons) as a flat list.
    fn get_processes(&self, app: &App) -> Vec<ProcessInfo> {
        let mut processes = Vec::new();

        for (group_name, group) in &app.state.groups {
            for task in &group.tasks {
                processes.push(ProcessInfo {
                    name: task.name.clone(),
                    group: group_name.clone(),
                    status: match task.status {
                        zaz_daemon::ProcessStatus::Pending => ProcessStatus::Pending,
                        zaz_daemon::ProcessStatus::Running => ProcessStatus::Running,
                        zaz_daemon::ProcessStatus::Success => ProcessStatus::Ready,
                        zaz_daemon::ProcessStatus::Failed => ProcessStatus::Failed,
                        zaz_daemon::ProcessStatus::Backoff => ProcessStatus::Pending,
                    },
                    kind: ProcessKind::Task,
                });
            }
            for daemon in &group.daemons {
                processes.push(ProcessInfo {
                    name: daemon.name.clone(),
                    group: group_name.clone(),
                    status: match daemon.status {
                        zaz_daemon::ProcessStatus::Pending => ProcessStatus::Pending,
                        zaz_daemon::ProcessStatus::Running => ProcessStatus::Running,
                        zaz_daemon::ProcessStatus::Success => ProcessStatus::Ready,
                        zaz_daemon::ProcessStatus::Failed => ProcessStatus::Failed,
                        zaz_daemon::ProcessStatus::Backoff => ProcessStatus::Pending,
                    },
                    kind: ProcessKind::Daemon,
                });
            }
        }

        processes
    }

    fn columns_for_count(&self, count: usize) -> usize {
        match count {
            0 | 1 => 1,
            2 => 2,
            3 | 4 => 2,
            _ => 3,
        }
    }

    fn hsplit(&self, area: Rect, count: usize) -> Vec<Rect> {
        let constraints: Vec<Constraint> = (0..count)
            .map(|_| Constraint::Ratio(1, count as u32))
            .collect();

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area)
            .to_vec()
    }

    fn vsplit(&self, area: Rect, count: usize) -> Vec<Rect> {
        let constraints: Vec<Constraint> = (0..count)
            .map(|_| Constraint::Ratio(1, count as u32))
            .collect();

        Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area)
            .to_vec()
    }

    fn grid(&self, area: Rect, cols: usize, rows: usize) -> Vec<Rect> {
        let row_constraints: Vec<Constraint> = (0..rows)
            .map(|_| Constraint::Ratio(1, rows as u32))
            .collect();

        let col_constraints: Vec<Constraint> = (0..cols)
            .map(|_| Constraint::Ratio(1, cols as u32))
            .collect();

        let row_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(area);

        let mut cells = Vec::with_capacity(cols * rows);
        for row in row_chunks.iter() {
            let col_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(col_constraints.clone())
                .split(*row);
            cells.extend(col_chunks.to_vec());
        }

        cells
    }

    fn draw_empty(&self, frame: &mut Frame, area: Rect) {
        let paragraph = Paragraph::new("No tasks or daemons configured")
            .block(Block::default().title(" Empty ").borders(Borders::ALL))
            .style(Style::default().fg(Color::DarkGray));

        frame.render_widget(paragraph, area);
    }

    fn draw_pane(
        &self,
        frame: &mut Frame,
        app: &App,
        area: Rect,
        process: &ProcessInfo,
        focused: bool,
    ) {
        let status_icon = match process.status {
            ProcessStatus::Pending => "○",
            ProcessStatus::Running => {
                if app.blink_on() {
                    "●"
                } else {
                    "○"
                }
            }
            ProcessStatus::Ready => "✓",
            ProcessStatus::Failed => "✗",
        };

        let status_color = match process.status {
            ProcessStatus::Pending => Color::White,
            ProcessStatus::Running => Color::Yellow,
            ProcessStatus::Ready => Color::Green,
            ProcessStatus::Failed => Color::Red,
        };

        let kind_indicator = match process.kind {
            ProcessKind::Task => "T",
            ProcessKind::Daemon => "D",
        };

        let title = format!(
            " {} {} {} ({}) ",
            status_icon, kind_indicator, process.name, process.group
        );

        let border_style = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        // Get logs for this process
        use crate::logs::timestamp_to_day;

        let logs = app.logs.filtered_logs(&process.name);
        let visible_height = area.height.saturating_sub(2) as usize;
        let total_lines = logs.len();
        let scroll_offset = total_lines.saturating_sub(visible_height);

        // Get reference day from first log (for day offset calculation)
        let reference_day = logs
            .first()
            .map(|(_, log)| timestamp_to_day(log.timestamp))
            .unwrap_or(0);

        let items: Vec<ListItem> = logs
            .iter()
            .skip(scroll_offset)
            .take(visible_height)
            .map(|(_idx, log)| {
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
                    Span::styled(content, line_style),
                ]))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .title(Span::styled(title, Style::default().fg(status_color)))
                .borders(Borders::ALL)
                .border_style(border_style),
        );

        frame.render_widget(list, area);
    }

    fn draw_status_bar(&self, frame: &mut Frame, app: &App, area: Rect, task_count: usize) {
        let connection_status = if app.is_connected() {
            Span::styled("●", Style::default().fg(Color::Green))
        } else {
            Span::styled("○", Style::default().fg(Color::Red))
        };

        let page_info = if task_count > 6 {
            let total_pages = (task_count + 5) / 6;
            format!(" Page {}/{} | ", app.current_page + 1, total_pages)
        } else {
            String::new()
        };

        let status = if let Some(ref msg) = app.status_message {
            msg.clone()
        } else {
            format!(" F2:Mini* |{}[q]uit [r]estart [?]help", page_info)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_style_name() {
        let style = MinimalStyle;
        assert_eq!(style.name(), "Minimal");
    }

    #[test]
    fn test_layout_single() {
        let style = MinimalStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 1);

        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].area, area);
    }

    #[test]
    fn test_layout_two_wide() {
        let style = MinimalStyle;
        // Wide terminal (3:1 ratio)
        let area = Rect::new(0, 0, 150, 50);
        let layouts = style.calculate_layout(area, 2);

        assert_eq!(layouts.len(), 2);
        // Should be horizontal split
        assert!(layouts[0].area.width > layouts[0].area.height);
    }

    #[test]
    fn test_layout_four() {
        let style = MinimalStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 4);

        assert_eq!(layouts.len(), 4);
    }

    #[test]
    fn test_layout_six() {
        let style = MinimalStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 6);

        assert_eq!(layouts.len(), 6);
    }

    #[test]
    fn test_layout_paginated() {
        let style = MinimalStyle;
        let area = Rect::new(0, 0, 100, 50);
        // More than 6 tasks should still only show 6 layouts
        let layouts = style.calculate_layout(area, 10);

        assert_eq!(layouts.len(), 6);
    }

    #[test]
    fn test_hsplit() {
        let style = MinimalStyle;
        let area = Rect::new(0, 0, 100, 50);
        let splits = style.hsplit(area, 3);

        assert_eq!(splits.len(), 3);
        assert!(splits[0].width < 50);
    }

    #[test]
    fn test_vsplit() {
        let style = MinimalStyle;
        let area = Rect::new(0, 0, 100, 50);
        let splits = style.vsplit(area, 2);

        assert_eq!(splits.len(), 2);
        assert!(splits[0].height < 30);
    }

    #[test]
    fn test_grid() {
        let style = MinimalStyle;
        let area = Rect::new(0, 0, 100, 50);
        let cells = style.grid(area, 2, 3);

        assert_eq!(cells.len(), 6);
    }
}
