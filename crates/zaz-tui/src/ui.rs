//! UI rendering.

use crate::app::{App, Focus};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

/// Draw the main UI.
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(frame.area());

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(chunks[0]);

    draw_groups(frame, app, left_chunks[0]);
    draw_status(frame, app, left_chunks[1]);
    draw_logs(frame, app, chunks[1]);
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
                zaz_daemon::GroupStatus::Running => "◐",
                zaz_daemon::GroupStatus::Ready => "●",
                zaz_daemon::GroupStatus::Failed => "✗",
            };

            let style = if i == app.selected_group {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(Line::from(vec![
                Span::raw(format!(" {} ", status_icon)),
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

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let status = format!(
        " Watching: {} files | [q]uit [r]estart [R]estart all ",
        app.state.watched_files
    );

    let paragraph =
        Paragraph::new(status).block(Block::default().title(" Status ").borders(Borders::ALL));

    frame.render_widget(paragraph, area);
}

fn draw_logs(frame: &mut Frame, app: &App, area: Rect) {
    let border_style = if app.focus == Focus::Logs {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let items: Vec<ListItem> = app
        .logs
        .iter()
        .map(|line| ListItem::new(Line::raw(line)))
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(" Logs ")
            .borders(Borders::ALL)
            .border_style(border_style),
    );

    frame.render_widget(list, area);
}
