//! Multi-pane style implementation.
//!
//! The multi-pane style shows one pane per task/daemon with adaptive layout:
//! - 1 task: Single full-screen pane
//! - 2 tasks: Horizontal or vertical split based on terminal aspect ratio
//! - 3-4 tasks: 2x2 grid
//! - 5-6 tasks: 3x2 or 2x3 grid based on terminal aspect ratio
//! - 7+ tasks: Paginated 2x3 grid (6 per page)
//!
//! # Navigation
//!
//! - j/k or up/down: Move between panes vertically
//! - h/l or left/right: Move between panes horizontally in grid
//! - Tab: Cycle through panes
//! - [/]: Previous/next page (when >6 tasks)
//! - g/G: Scroll focused pane to top/bottom
//!
//! # Features
//!
//! - Per-pane scroll positions (each pane scrolls independently)
//! - Per-pane follow mode (auto-scroll to bottom)
//! - Search highlights in all panes
//! - Filter applies to all panes

use super::{KeyResult, PaneLayout, SelectedProcess, StyleRenderer};
use crate::ansi;
use crate::app::{App, ConnectionStatus, Focus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

/// Multi-pane style renderer with one pane per task.
pub struct MultiPaneStyle;

impl StyleRenderer for MultiPaneStyle {
    fn draw(&self, frame: &mut Frame, app: &mut App) {
        let area = frame.area();

        // Reserve space for status bar
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        let main_area = chunks[0];
        let status_area = chunks[1];

        // Auto-initialize panes_per_page based on terminal width
        if app.panes_per_page == 0 {
            let is_wide = (main_area.width as f32 / main_area.height as f32) > 2.0;
            app.panes_per_page = if is_wide { 3 } else { 2 };
        }
        let panes_per_page = app.panes_per_page;

        // Get all tasks/daemons
        let processes = self.get_processes(app);
        let task_count = processes.len();

        if task_count == 0 {
            self.draw_empty(frame, main_area);
        } else {
            let layouts =
                self.calculate_layout_for_panes(main_area, panes_per_page.min(task_count));

            // Calculate which tasks to show on current page
            let start_idx = app.current_page * panes_per_page;
            let end_idx = (start_idx + panes_per_page).min(task_count);
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

                // Update pane visible height for scroll calculations
                let visible_height = pane_layout.area.height.saturating_sub(2) as usize;
                app.pane_visible_height.insert(global_idx, visible_height);

                self.draw_pane(
                    frame,
                    app,
                    pane_layout.area,
                    process,
                    global_idx,
                    is_focused,
                );
            }
        }

        self.draw_status_bar(frame, app, status_area, task_count);
    }

    fn handle_key(&self, app: &mut App, key: KeyEvent) -> KeyResult {
        let task_count = app.task_count();

        // Handle Ctrl+key combinations first
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    // Half page down in focused pane
                    if let Some(process) = self.get_process_at_index(app, app.selected_pane) {
                        let pane = app.selected_pane;
                        let total = app.logs.total_count(&process.name);
                        let visible_height =
                            app.pane_visible_height.get(&pane).copied().unwrap_or(20);
                        let half_page = visible_height / 2;
                        let max_scroll = total.saturating_sub(visible_height);

                        // Sync scroll position from follow mode before disabling
                        let is_following = app.pane_follow.get(&pane).copied().unwrap_or(true);
                        let current = if is_following {
                            max_scroll
                        } else {
                            app.get_pane_scroll(pane)
                        };
                        app.pane_follow.insert(pane, false);

                        app.set_pane_scroll(pane, (current + half_page).min(max_scroll));
                    }
                    return KeyResult::Handled;
                }
                KeyCode::Char('u') => {
                    // Half page up in focused pane
                    if let Some(process) = self.get_process_at_index(app, app.selected_pane) {
                        let pane = app.selected_pane;
                        let total = app.logs.total_count(&process.name);
                        let visible_height =
                            app.pane_visible_height.get(&pane).copied().unwrap_or(20);
                        let half_page = visible_height / 2;
                        let max_scroll = total.saturating_sub(visible_height);

                        // Sync scroll position from follow mode before disabling
                        let is_following = app.pane_follow.get(&pane).copied().unwrap_or(true);
                        let current = if is_following {
                            max_scroll
                        } else {
                            app.get_pane_scroll(pane)
                        };
                        app.pane_follow.insert(pane, false);

                        app.set_pane_scroll(pane, current.saturating_sub(half_page));
                    }
                    return KeyResult::Handled;
                }
                _ => {}
            }
        }

        match key.code {
            // Log scrolling (j/k scroll the focused pane)
            KeyCode::Char('j') => {
                self.scroll_pane(app, 1);
                KeyResult::Handled
            }
            KeyCode::Char('k') => {
                self.scroll_pane(app, -1);
                KeyResult::Handled
            }

            // Grid/pane navigation (arrows, h/l, Tab)
            KeyCode::Down => {
                self.navigate_vertical(app, 1);
                KeyResult::Handled
            }
            KeyCode::Up => {
                self.navigate_vertical(app, -1);
                KeyResult::Handled
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.navigate_horizontal(app, -1);
                KeyResult::Handled
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.navigate_horizontal(app, 1);
                KeyResult::Handled
            }
            KeyCode::Tab => {
                if task_count > 0 {
                    let panes_per_page = app.panes_per_page.max(1);
                    app.selected_pane = (app.selected_pane + 1) % task_count;
                    app.current_page = app.selected_pane / panes_per_page;
                    app.focus = Focus::Pane(app.selected_pane);
                }
                KeyResult::Handled
            }
            KeyCode::BackTab => {
                // Shift+Tab: cycle panes in reverse
                if task_count > 0 {
                    let panes_per_page = app.panes_per_page.max(1);
                    app.selected_pane = if app.selected_pane == 0 {
                        task_count - 1
                    } else {
                        app.selected_pane - 1
                    };
                    app.current_page = app.selected_pane / panes_per_page;
                    app.focus = Focus::Pane(app.selected_pane);
                }
                KeyResult::Handled
            }

            // Pagination
            KeyCode::Char('[') => {
                let panes_per_page = app.panes_per_page.max(1);
                if app.current_page > 0 {
                    app.current_page -= 1;
                    app.selected_pane = app.current_page * panes_per_page;
                    app.focus = Focus::Pane(app.selected_pane);
                }
                KeyResult::Handled
            }
            KeyCode::Char(']') => {
                let panes_per_page = app.panes_per_page.max(1);
                let max_page = task_count.saturating_sub(1) / panes_per_page;
                if app.current_page < max_page {
                    app.current_page += 1;
                    app.selected_pane = app.current_page * panes_per_page;
                    app.focus = Focus::Pane(app.selected_pane);
                }
                KeyResult::Handled
            }

            // Adjust panes per page
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if app.panes_per_page < 6 {
                    app.panes_per_page += 1;
                    // Recalculate current page to keep selected pane visible
                    app.current_page = app.selected_pane / app.panes_per_page;
                }
                KeyResult::Handled
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                if app.panes_per_page > 1 {
                    app.panes_per_page -= 1;
                    // Recalculate current page to keep selected pane visible
                    app.current_page = app.selected_pane / app.panes_per_page;
                }
                KeyResult::Handled
            }

            // Per-pane scrolling
            KeyCode::Char('g') => {
                app.set_pane_scroll(app.selected_pane, 0);
                app.pane_follow.insert(app.selected_pane, false);
                KeyResult::Handled
            }
            KeyCode::Char('G') => {
                // Scroll to bottom of focused pane
                if let Some(process) = self.get_process_at_index(app, app.selected_pane) {
                    let total = app.logs.total_count(&process.name);
                    let visible_height = app
                        .pane_visible_height
                        .get(&app.selected_pane)
                        .copied()
                        .unwrap_or(20);
                    let max_scroll = total.saturating_sub(visible_height);
                    app.set_pane_scroll(app.selected_pane, max_scroll);
                    app.pane_follow.insert(app.selected_pane, true);
                }
                KeyResult::Handled
            }
            KeyCode::PageUp => {
                if let Some(process) = self.get_process_at_index(app, app.selected_pane) {
                    let pane = app.selected_pane;
                    let total = app.logs.total_count(&process.name);
                    let visible_height = app.pane_visible_height.get(&pane).copied().unwrap_or(20);
                    let max_scroll = total.saturating_sub(visible_height);

                    // Sync scroll position from follow mode before disabling
                    let is_following = app.pane_follow.get(&pane).copied().unwrap_or(true);
                    let current = if is_following {
                        max_scroll
                    } else {
                        app.get_pane_scroll(pane)
                    };
                    app.pane_follow.insert(pane, false);

                    app.set_pane_scroll(pane, current.saturating_sub(visible_height));
                }
                KeyResult::Handled
            }
            KeyCode::PageDown => {
                if let Some(process) = self.get_process_at_index(app, app.selected_pane) {
                    let pane = app.selected_pane;
                    let total = app.logs.total_count(&process.name);
                    let visible_height = app.pane_visible_height.get(&pane).copied().unwrap_or(20);
                    let max_scroll = total.saturating_sub(visible_height);

                    // Sync scroll position from follow mode before disabling
                    let is_following = app.pane_follow.get(&pane).copied().unwrap_or(true);
                    let current = if is_following {
                        max_scroll
                    } else {
                        app.get_pane_scroll(pane)
                    };
                    app.pane_follow.insert(pane, false);

                    app.set_pane_scroll(pane, (current + visible_height).min(max_scroll));
                }
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
                app.pane_scroll.clear();
                KeyResult::SetStatus("Logs cleared".to_string())
            }

            // Follow mode (per-pane)
            KeyCode::Char('F') => {
                let pane = app.selected_pane;
                let current = app.pane_follow.get(&pane).copied().unwrap_or(true);
                app.pane_follow.insert(pane, !current);
                let status = if !current {
                    "Follow mode ON (this pane)"
                } else {
                    "Follow mode OFF (this pane)"
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
        "Multi Pane"
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
                    self.grid(area, 3, 2)
                } else {
                    self.grid(area, 2, 3)
                }
            }
            _ => self.grid(area, 2, 3),
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

    fn get_selected_process(&self, app: &App) -> Option<SelectedProcess> {
        self.get_process_at_index(app, app.selected_pane)
            .map(|p| SelectedProcess {
                group: p.group.clone(),
                process: p.name.clone(),
                is_group: false,
            })
    }

    fn on_activate(&self, app: &mut App) {
        app.focus = Focus::Pane(app.selected_pane);
    }

    fn log_dimensions(&self, app: &App) -> (usize, usize) {
        if let Some(process) = self.get_process_at_index(app, app.selected_pane) {
            let total = app.logs.total_count(&process.name);
            let visible_height = app
                .pane_visible_height
                .get(&app.selected_pane)
                .copied()
                .unwrap_or(20);
            (visible_height, total)
        } else {
            (20, 0)
        }
    }
}

/// Process info for rendering.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub name: String,
    pub group: String,
    pub status: ProcessStatus,
    pub kind: ProcessKind,
    pub duration_ms: Option<u64>,
    pub pid: Option<u32>,
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

impl MultiPaneStyle {
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
                    duration_ms: task.duration_ms,
                    pid: None,
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
                    duration_ms: None,
                    pid: daemon.pid,
                });
            }
        }

        processes
    }

    /// Get process at a specific index.
    fn get_process_at_index(&self, app: &App, index: usize) -> Option<ProcessInfo> {
        self.get_processes(app).into_iter().nth(index)
    }

    /// Scroll the focused pane by the given amount (positive = down, negative = up).
    fn scroll_pane(&self, app: &mut App, direction: i32) {
        let pane = app.selected_pane;

        // Get the process for this pane to calculate log count
        if let Some(process) = self.get_process_at_index(app, pane) {
            let total = app.logs.total_count(&process.name);
            let visible_height = app.pane_visible_height.get(&pane).copied().unwrap_or(20);
            let max_scroll = total.saturating_sub(visible_height);

            // Sync scroll position from follow mode before disabling
            let is_following = app.pane_follow.get(&pane).copied().unwrap_or(true);
            let current = if is_following {
                max_scroll
            } else {
                app.get_pane_scroll(pane)
            };

            let new_scroll = if direction > 0 {
                (current + 1).min(max_scroll)
            } else {
                current.saturating_sub(1)
            };

            app.set_pane_scroll(pane, new_scroll);

            // Disable follow mode when manually scrolling
            if new_scroll < max_scroll {
                app.pane_follow.insert(pane, false);
            }
        }
    }

    /// Get the number of columns for a given pane count.
    /// Prioritizes horizontal layout to give each pane more vertical space.
    fn columns_for_panes(&self, panes: usize) -> usize {
        match panes {
            0 | 1 => 1,
            2 => 2,
            3 => 3,
            4 => 2, // 2x2 grid
            5 => 3, // 3+2 layout
            6 => 3, // 3x2 grid
            _ => 3,
        }
    }

    /// Calculate layout for a specific number of panes.
    fn calculate_layout_for_panes(&self, area: Rect, panes: usize) -> Vec<PaneLayout> {
        if panes == 0 {
            return vec![];
        }

        let rects = match panes {
            1 => vec![area],
            2 => self.hsplit(area, 2),
            3 => self.hsplit(area, 3),
            4 => self.grid(area, 2, 2),
            5 => {
                // 3 on top, 2 on bottom
                let rows = self.vsplit(area, 2);
                let mut cells = self.hsplit(rows[0], 3);
                cells.extend(self.hsplit(rows[1], 2));
                cells
            }
            6 => self.grid(area, 3, 2),
            _ => self.grid(area, 3, 2),
        };

        rects
            .into_iter()
            .take(panes)
            .map(|r| PaneLayout {
                area: r,
                process: None,
                focused: false,
            })
            .collect()
    }

    /// Get the number of columns in the current grid layout (legacy, for tests).
    #[allow(dead_code)]
    fn columns_for_count(&self, count: usize) -> usize {
        self.columns_for_panes(count.min(6))
    }

    /// Get the number of rows in the current grid layout (legacy, for tests).
    #[allow(dead_code)]
    fn rows_for_count(&self, count: usize) -> usize {
        match count.min(6) {
            0 | 1 => 1,
            2 | 3 => 1,
            4..=6 => 2,
            _ => 2,
        }
    }

    /// Navigate vertically in the grid.
    fn navigate_vertical(&self, app: &mut App, direction: i32) {
        let task_count = app.task_count();
        if task_count == 0 {
            return;
        }

        let panes_per_page = app.panes_per_page.max(1);
        let visible_count = task_count.min(panes_per_page);
        let cols = self.columns_for_panes(panes_per_page);
        let page_offset = app.current_page * panes_per_page;
        let local_idx = app.selected_pane.saturating_sub(page_offset);

        let new_local = if direction > 0 {
            // Move down
            let next = local_idx + cols;
            if next < visible_count {
                next
            } else {
                // Wrap to next page or first item
                if app.current_page * panes_per_page + visible_count < task_count {
                    app.current_page += 1;
                    0
                } else {
                    local_idx % cols
                }
            }
        } else {
            // Move up
            if local_idx >= cols {
                local_idx - cols
            } else {
                // Wrap to previous page or last row
                if app.current_page > 0 {
                    app.current_page -= 1;
                    let prev_visible =
                        panes_per_page.min(task_count - app.current_page * panes_per_page);
                    let rows = prev_visible.div_ceil(cols);
                    let target_row = rows - 1;
                    (target_row * cols + local_idx).min(prev_visible - 1)
                } else {
                    let rows = visible_count.div_ceil(cols);
                    let target_row = rows - 1;
                    (target_row * cols + local_idx).min(visible_count - 1)
                }
            }
        };

        app.selected_pane = app.current_page * panes_per_page + new_local;
        app.focus = Focus::Pane(app.selected_pane);
    }

    /// Navigate horizontally in the grid.
    fn navigate_horizontal(&self, app: &mut App, direction: i32) {
        let task_count = app.task_count();
        if task_count == 0 {
            return;
        }

        let panes_per_page = app.panes_per_page.max(1);
        let visible_count = task_count.min(panes_per_page);
        let cols = self.columns_for_panes(panes_per_page);
        let page_offset = app.current_page * panes_per_page;
        let local_idx = app.selected_pane.saturating_sub(page_offset);
        let row = local_idx / cols;
        let col = local_idx % cols;

        let new_col = if direction > 0 {
            // Move right
            if col + 1 < cols && local_idx + 1 < visible_count {
                col + 1
            } else {
                0 // Wrap to start of row
            }
        } else {
            // Move left
            if col > 0 {
                col - 1
            } else {
                // Wrap to end of row
                let row_end = ((row + 1) * cols - 1).min(visible_count - 1);
                row_end % cols
            }
        };

        let new_local = row * cols + new_col;
        if new_local < visible_count {
            app.selected_pane = app.current_page * panes_per_page + new_local;
            app.focus = Focus::Pane(app.selected_pane);
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
        pane_index: usize,
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

        use crate::logs::timestamp_to_day;

        let total_lines = app.logs.total_count(&process.name);
        let visible_height = area.height.saturating_sub(2) as usize;

        // Per-pane scroll handling
        let is_following = app.pane_follow.get(&pane_index).copied().unwrap_or(true);
        let scroll_offset = if is_following {
            total_lines.saturating_sub(visible_height)
        } else {
            let stored = app.get_pane_scroll(pane_index);
            stored.min(total_lines.saturating_sub(visible_height))
        };

        let display_lines =
            app.logs
                .get_display_lines(&process.name, scroll_offset, visible_height);

        // Format title with process info
        let info = match process.kind {
            ProcessKind::Task => {
                if let Some(ms) = process.duration_ms {
                    format!("({:.1}s)", ms as f64 / 1000.0)
                } else {
                    String::new()
                }
            }
            ProcessKind::Daemon => {
                if let Some(pid) = process.pid {
                    format!("(pid {})", pid)
                } else {
                    String::new()
                }
            }
        };

        // Add asterisk for focused pane
        let focus_indicator = if focused { "*" } else { "" };

        // Add loading indicator and line range
        let is_loading = app.pane_loading.get(&pane_index).copied().unwrap_or(false);
        let loading_indicator = if is_loading { " loading" } else { "" };

        let line_range = if total_lines > 0 {
            let start = scroll_offset + 1;
            let end = (scroll_offset + visible_height).min(total_lines);
            format!(" ({}-{}/{}{})", start, end, total_lines, loading_indicator)
        } else {
            " (empty)".to_string()
        };

        let title = format!(
            " [{}] {}{} {}{}",
            status_icon, process.name, focus_indicator, info, line_range
        )
        .trim_end()
        .to_string()
            + " ";

        let border_style = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let reference_day = display_lines
            .iter()
            .find_map(|l| l.as_ref())
            .map(|l| timestamp_to_day(l.log.timestamp))
            .unwrap_or(0);

        let items: Vec<ListItem> = display_lines
            .iter()
            .map(|entry| match entry {
                Some(paged) => {
                    let is_match = app.logs.is_search_match(&paged.log.content);
                    let is_daemon_log = paged.log.source == crate::daemon::LogSource::Daemon;

                    let timestamp = paged
                        .log
                        .format_timestamp(reference_day, app.show_full_timestamp);

                    // Build prefix spans (timestamp only for multi-pane)
                    let prefix_spans = vec![Span::styled(
                        format!("{} ", timestamp),
                        Style::default().fg(Color::DarkGray),
                    )];

                    let line = if is_daemon_log {
                        let mut spans = prefix_spans;
                        spans.push(Span::styled(
                            format!("[zaz] {}", paged.log.content),
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::DIM),
                        ));
                        Line::from(spans)
                    } else if app.user_config.log_colors.preserve_ansi {
                        ansi::parse_ansi_with_prefix(prefix_spans, &paged.log.content)
                    } else {
                        let mut spans = prefix_spans;
                        spans.push(Span::raw(paged.log.content.clone()));
                        Line::from(spans)
                    };

                    if is_match {
                        ListItem::new(line).style(Style::default().add_modifier(Modifier::REVERSED))
                    } else {
                        ListItem::new(line)
                    }
                }
                None => {
                    // Loading placeholder for uncached lines
                    ListItem::new(Line::from(vec![Span::styled(
                        "  Loading...",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    )]))
                }
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

        let panes_per_page = app.panes_per_page.max(1);
        let page_info = if task_count == 0 {
            " No tasks |".to_string()
        } else {
            let start_pane = app.current_page * panes_per_page + 1;
            let end_pane = (start_pane + panes_per_page - 1).min(task_count);
            let pane_range = if start_pane == end_pane {
                format!("{}", start_pane)
            } else {
                format!("{}-{}", start_pane, end_pane)
            };
            if task_count > panes_per_page {
                let total_pages = task_count.div_ceil(panes_per_page);
                format!(
                    " Page {}/{}, Panes {}/{} |",
                    app.current_page + 1,
                    total_pages,
                    pane_range,
                    task_count
                )
            } else {
                format!(" Panes {}/{} |", pane_range, task_count)
            }
        };

        // Show follow status for focused pane
        let is_following = app
            .pane_follow
            .get(&app.selected_pane)
            .copied()
            .unwrap_or(true);

        let follow_icon = if is_following { "✓" } else { "○" };
        let follow_style = if is_following {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let filter_status = if app.logs.has_filter() {
            format!(" │ filter: {}", app.logs.filter_pattern().unwrap_or(""))
        } else {
            String::new()
        };

        // Build connection status text based on state
        let (connection_text, connection_text_style) = match app.connection_status {
            ConnectionStatus::Loaded => (app.config_name.clone(), Style::default()),
            ConnectionStatus::Disconnected => {
                ("Disconnected".to_string(), Style::default().fg(Color::Red))
            }
            ConnectionStatus::Reconnected => (
                "Reconnected".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            ConnectionStatus::Initial => (
                "Connecting...".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        };

        let mut lines = vec![Line::from(vec![
            Span::raw(" "),
            connection_status,
            Span::raw(" "),
            Span::styled(connection_text, connection_text_style),
            Span::raw(" │"),
            Span::raw(page_info),
            Span::raw(" Follow "),
            Span::styled(follow_icon, follow_style),
            Span::raw(&filter_status),
            Span::raw(" │ [q]uit [r]estart [?]help"),
        ])];

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
    fn test_multi_pane_style_name() {
        let style = MultiPaneStyle;
        assert_eq!(style.name(), "Multi Pane");
    }

    #[test]
    fn test_layout_single() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 1);

        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].area, area);
    }

    #[test]
    fn test_layout_two_wide() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 150, 50);
        let layouts = style.calculate_layout(area, 2);

        assert_eq!(layouts.len(), 2);
        assert!(layouts[0].area.width > layouts[0].area.height);
    }

    #[test]
    fn test_layout_four() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 4);

        assert_eq!(layouts.len(), 4);
    }

    #[test]
    fn test_layout_six() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 6);

        assert_eq!(layouts.len(), 6);
    }

    #[test]
    fn test_layout_paginated() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 100, 50);
        let layouts = style.calculate_layout(area, 10);

        assert_eq!(layouts.len(), 6);
    }

    #[test]
    fn test_columns_for_count() {
        let style = MultiPaneStyle;
        assert_eq!(style.columns_for_count(1), 1);
        assert_eq!(style.columns_for_count(2), 2);
        assert_eq!(style.columns_for_count(4), 2);
        assert_eq!(style.columns_for_count(6), 3);
    }

    #[test]
    fn test_hsplit() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 100, 50);
        let splits = style.hsplit(area, 3);

        assert_eq!(splits.len(), 3);
        assert!(splits[0].width < 50);
    }

    #[test]
    fn test_vsplit() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 100, 50);
        let splits = style.vsplit(area, 2);

        assert_eq!(splits.len(), 2);
        assert!(splits[0].height < 30);
    }

    #[test]
    fn test_grid() {
        let style = MultiPaneStyle;
        let area = Rect::new(0, 0, 100, 50);
        let cells = style.grid(area, 2, 3);

        assert_eq!(cells.len(), 6);
    }
}
