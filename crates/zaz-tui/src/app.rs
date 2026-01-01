//! Application state and logic.

use crate::{events, Event, TuiError};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::time::Duration;
use zaz_daemon::DaemonState;

/// Main application state.
pub struct App {
    /// Whether the app should quit.
    pub should_quit: bool,

    /// Currently selected group index.
    pub selected_group: usize,

    /// Daemon state (updated periodically).
    pub state: DaemonState,

    /// Log lines for display.
    pub logs: Vec<String>,

    /// Currently focused pane.
    pub focus: Focus,
}

/// Which pane is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Groups,
    Logs,
}

impl App {
    /// Create a new application.
    pub fn new() -> Self {
        Self {
            should_quit: false,
            selected_group: 0,
            state: DaemonState::default(),
            logs: Vec::new(),
            focus: Focus::Groups,
        }
    }

    /// Handle an event.
    pub fn handle_event(&mut self, event: Event) {
        use crossterm::event::KeyCode;

        if events::is_quit(&event) {
            self.should_quit = true;
            return;
        }

        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.state.groups.is_empty() {
                        self.selected_group = (self.selected_group + 1) % self.state.groups.len();
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !self.state.groups.is_empty() {
                        self.selected_group = self
                            .selected_group
                            .checked_sub(1)
                            .unwrap_or(self.state.groups.len().saturating_sub(1));
                    }
                }
                KeyCode::Tab => {
                    self.focus = match self.focus {
                        Focus::Groups => Focus::Logs,
                        Focus::Logs => Focus::Groups,
                    };
                }
                KeyCode::Char('r') => {
                    // TODO: send restart request
                    self.logs.push("Restart requested".to_string());
                }
                KeyCode::Char('R') => {
                    // TODO: send restart all request
                    self.logs.push("Restart all requested".to_string());
                }
                KeyCode::Char('c') => {
                    self.logs.clear();
                }
                _ => {}
            }
        }
    }

    /// Run the TUI event loop.
    pub fn run(&mut self) -> Result<(), TuiError> {
        let mut terminal = setup_terminal()?;

        let result = self.event_loop(&mut terminal);

        restore_terminal(&mut terminal)?;
        result
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), TuiError> {
        let tick_rate = Duration::from_millis(100);

        while !self.should_quit {
            terminal.draw(|frame| crate::ui::draw(frame, self))?;

            if let Some(event) = events::poll(tick_rate)? {
                self.handle_event(event);
            }
        }

        Ok(())
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, TuiError> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), TuiError> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
