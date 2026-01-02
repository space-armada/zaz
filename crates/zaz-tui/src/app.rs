//! Application state and logic.

use crate::daemon::{ClientCommand, DaemonConnection};
use crate::logs::LogBuffer;
use crate::{events, Event, TuiError};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::path::Path;
use std::time::Duration;
use zaz_config::{TuiStylePreference, UserConfig};
use zaz_daemon::DaemonState;

/// TUI display style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TuiStyle {
    /// Full style with group tree and logs pane.
    #[default]
    Full,
    /// Minimal style with one pane per task.
    Minimal,
}

impl From<TuiStylePreference> for TuiStyle {
    fn from(pref: TuiStylePreference) -> Self {
        match pref {
            TuiStylePreference::Full => TuiStyle::Full,
            TuiStylePreference::Minimal => TuiStyle::Minimal,
        }
    }
}

/// Current input mode for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Normal navigation mode.
    #[default]
    Normal,
    /// Filtering logs.
    Filter,
    /// Searching logs.
    Search,
    /// Confirming quit (when we started the daemon).
    QuitPrompt,
}

/// Which pane is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Groups tree (full style).
    #[default]
    Groups,
    /// Logs pane.
    Logs,
    /// Task pane (minimal style).
    Pane(usize),
}

/// Describes what is currently selected in the groups pane.
#[derive(Debug, Clone)]
enum SelectedItem {
    /// A group header is selected.
    Group(String),
    /// A task within a group is selected.
    Task { group: String, task: String },
    /// A daemon within a group is selected.
    Daemon { group: String, daemon: String },
}

/// Main application state.
pub struct App {
    // === Core ===
    /// Config file name (e.g., "zaz.toml").
    pub config_name: String,
    /// Current display style.
    pub style: TuiStyle,
    /// Daemon state (updated periodically).
    pub state: DaemonState,
    /// Connection to the daemon.
    pub daemon: Option<DaemonConnection>,
    /// Log buffer with filtering/search.
    pub logs: LogBuffer,

    // === UI State ===
    /// Currently focused element.
    pub focus: Focus,
    /// Selected item index in flat list (groups + tasks + daemons).
    pub selected_item: usize,
    /// Selected pane index (minimal style).
    pub selected_pane: usize,
    /// Current page for pagination (minimal style, >6 tasks).
    pub current_page: usize,
    /// Log scroll offset.
    pub log_scroll: usize,
    /// Whether to show full timestamps (vs compact time-only).
    pub show_full_timestamp: bool,

    // === Input Modes ===
    /// Current input mode.
    pub input_mode: InputMode,
    /// Filter input buffer.
    pub filter_input: String,
    /// Search input buffer.
    pub search_input: String,

    // === Animation ===
    /// Animation tick counter (0-255, wraps).
    pub animation_tick: u8,
    /// User configuration.
    pub user_config: UserConfig,

    // === Lifecycle ===
    /// Whether we started the daemon (affects quit behavior).
    pub started_daemon: bool,
    /// Whether the app should quit.
    pub should_quit: bool,
    /// Status message to display.
    pub status_message: Option<String>,
    /// Whether to show help overlay.
    pub show_help: bool,
}

impl App {
    /// Create a new application with the given style, user config, and config file name.
    pub fn new(style: TuiStyle, user_config: UserConfig, config_name: String) -> Self {
        Self {
            config_name,
            style,
            state: DaemonState::default(),
            daemon: None,
            logs: LogBuffer::new(),
            focus: Focus::Groups,
            selected_item: 0,
            selected_pane: 0,
            current_page: 0,
            log_scroll: 0,
            show_full_timestamp: false,
            input_mode: InputMode::Normal,
            filter_input: String::new(),
            search_input: String::new(),
            animation_tick: 0,
            user_config,
            started_daemon: false,
            should_quit: false,
            status_message: None,
            show_help: false,
        }
    }

    /// Connect to the daemon.
    pub async fn connect(&mut self, socket_path: &Path) -> Result<(), TuiError> {
        let connection = DaemonConnection::connect(socket_path).await?;
        self.daemon = Some(connection);
        Ok(())
    }

    /// Check if connected to the daemon.
    pub fn is_connected(&self) -> bool {
        self.daemon
            .as_ref()
            .map(|d| d.is_connected())
            .unwrap_or(false)
    }

    /// Get the total number of tasks/daemons.
    pub fn task_count(&self) -> usize {
        self.state
            .groups
            .values()
            .map(|g| g.tasks.len() + g.daemons.len())
            .sum()
    }

    /// Get the number of pages needed for minimal style.
    pub fn page_count(&self) -> usize {
        let count = self.task_count();
        if count <= 6 {
            1
        } else {
            (count + 5) / 6 // Ceiling division
        }
    }

    /// Set a temporary status message.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    /// Clear the status message.
    pub fn clear_status(&mut self) {
        self.status_message = None;
    }

    /// Toggle the display style.
    pub fn toggle_style(&mut self) {
        self.style = match self.style {
            TuiStyle::Full => TuiStyle::Minimal,
            TuiStyle::Minimal => TuiStyle::Full,
        };
        // Reset focus based on new style
        self.focus = match self.style {
            TuiStyle::Full => Focus::Groups,
            TuiStyle::Minimal => Focus::Pane(0),
        };
    }

    /// Toggle timestamp display mode (compact vs full).
    pub fn toggle_timestamp(&mut self) {
        self.show_full_timestamp = !self.show_full_timestamp;
    }

    /// Poll for updates from the daemon.
    pub fn poll_daemon(&mut self) {
        if let Some(ref mut daemon) = self.daemon {
            // Receive state updates
            while let Some(state) = daemon.try_recv_state() {
                self.state = state;
            }

            // Receive log lines
            while let Some(log) = daemon.try_recv_log() {
                self.logs.push(log);
            }
        }
    }

    /// Send a command to the daemon.
    pub fn send_command(&mut self, cmd: ClientCommand) {
        if let Some(ref daemon) = self.daemon {
            if let Err(e) = daemon.send_command(cmd) {
                self.set_status(format!("Command failed: {}", e));
            }
        } else {
            self.set_status("Not connected to daemon");
        }
    }

    /// Handle an event.
    pub fn handle_event(&mut self, event: Event) {
        use crossterm::event::KeyCode;

        // Handle help toggle first
        if self.show_help {
            if let Event::Key(key) = &event {
                if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                    self.show_help = false;
                    return;
                }
            }
            // Ignore other keys when help is showing
            return;
        }

        // Handle quit regardless of mode
        if events::is_quit(&event) {
            if self.started_daemon {
                // If we started the daemon, shut it down when quitting
                self.send_command(ClientCommand::Shutdown);
            }
            self.should_quit = true;
            return;
        }

        // Handle events based on input mode
        match self.input_mode {
            InputMode::Normal => self.handle_normal_mode(event),
            InputMode::Filter => self.handle_filter_mode(event),
            InputMode::Search => self.handle_search_mode(event),
            InputMode::QuitPrompt => {
                // Legacy: no longer used, but kept for enum completeness
                self.input_mode = InputMode::Normal;
            }
        }
    }

    fn handle_normal_mode(&mut self, event: Event) {
        use crossterm::event::KeyCode;

        if let Event::Key(key) = event {
            match key.code {
                // Navigation
                KeyCode::Char('j') | KeyCode::Down => self.navigate_down(),
                KeyCode::Char('k') | KeyCode::Up => self.navigate_up(),
                KeyCode::Char('h') | KeyCode::Left => self.navigate_left(),
                KeyCode::Char('l') | KeyCode::Right => self.navigate_right(),
                KeyCode::Tab => self.cycle_focus(),

                // Style switching
                KeyCode::F(1) => {
                    self.style = TuiStyle::Full;
                    self.focus = Focus::Groups;
                }
                KeyCode::F(2) => {
                    self.style = TuiStyle::Minimal;
                    self.focus = Focus::Pane(0);
                }

                // Actions
                KeyCode::Char('r') => self.restart_selected(),
                KeyCode::Char('R') => self.restart_all(),
                KeyCode::Char('c') => self.clear_logs(),

                // Log navigation
                KeyCode::Char('g') => self.scroll_to_top(),
                KeyCode::Char('G') => self.scroll_to_bottom(),
                KeyCode::PageUp => self.page_up(),
                KeyCode::PageDown => self.page_down(),

                // Modes
                KeyCode::Char('f') => {
                    self.input_mode = InputMode::Filter;
                    self.filter_input.clear();
                }
                KeyCode::Char('/') => {
                    self.input_mode = InputMode::Search;
                    self.search_input.clear();
                }
                KeyCode::Char('n') => {
                    if let Some(line) = self.logs.next_search_match() {
                        self.log_scroll = line;
                    }
                }
                KeyCode::Char('N') => {
                    if let Some(line) = self.logs.prev_search_match() {
                        self.log_scroll = line;
                    }
                }
                KeyCode::Char('?') => {
                    self.show_help = true;
                }

                // Pagination (minimal style)
                KeyCode::Char('[') => self.prev_page(),
                KeyCode::Char(']') => self.next_page(),

                // Follow mode toggle
                KeyCode::Char('F') => {
                    self.logs.toggle_follow();
                    let status = if self.logs.is_following() {
                        "Follow mode ON"
                    } else {
                        "Follow mode OFF"
                    };
                    self.set_status(status);
                }

                // Timestamp display toggle
                KeyCode::Char('t') => {
                    self.toggle_timestamp();
                    let status = if self.show_full_timestamp {
                        "Full timestamps"
                    } else {
                        "Compact timestamps"
                    };
                    self.set_status(status);
                }

                KeyCode::Esc => {
                    self.logs.clear_filter();
                    self.logs.clear_search();
                    self.clear_status();
                }

                _ => {}
            }
        }
    }

    fn handle_filter_mode(&mut self, event: Event) {
        use crossterm::event::KeyCode;

        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Enter => {
                    if let Err(e) = self.logs.set_filter(&self.filter_input) {
                        self.set_status(format!("Invalid filter: {}", e));
                    }
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Esc => {
                    self.filter_input.clear();
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    self.filter_input.pop();
                }
                KeyCode::Char(c) => {
                    self.filter_input.push(c);
                }
                _ => {}
            }
        }
    }

    fn handle_search_mode(&mut self, event: Event) {
        use crossterm::event::KeyCode;

        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Enter => {
                    if let Err(e) = self.logs.start_search(&self.search_input) {
                        self.set_status(format!("Invalid search: {}", e));
                    } else if let Some(line) = self.logs.next_search_match() {
                        self.log_scroll = line;
                    }
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Esc => {
                    self.search_input.clear();
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    self.search_input.pop();
                }
                KeyCode::Char(c) => {
                    self.search_input.push(c);
                }
                _ => {}
            }
        }
    }

    /// Count total items in the groups pane (groups + tasks + daemons).
    fn groups_item_count(&self) -> usize {
        self.state
            .groups
            .values()
            .map(|g| 1 + g.tasks.len() + g.daemons.len())
            .sum()
    }

    /// Get detailed info about the currently selected item.
    fn selected_item_info(&self) -> Option<SelectedItem> {
        let mut idx = 0;
        for group in self.state.groups.values() {
            // Check if group header is selected
            if self.selected_item == idx {
                return Some(SelectedItem::Group(group.name.clone()));
            }
            idx += 1;

            // Check tasks
            for task in &group.tasks {
                if self.selected_item == idx {
                    return Some(SelectedItem::Task {
                        group: group.name.clone(),
                        task: task.name.clone(),
                    });
                }
                idx += 1;
            }

            // Check daemons
            for daemon in &group.daemons {
                if self.selected_item == idx {
                    return Some(SelectedItem::Daemon {
                        group: group.name.clone(),
                        daemon: daemon.name.clone(),
                    });
                }
                idx += 1;
            }
        }
        None
    }

    fn navigate_down(&mut self) {
        match self.style {
            TuiStyle::Full => {
                match self.focus {
                    Focus::Groups => {
                        let total = self.groups_item_count();
                        if total > 0 {
                            self.selected_item = (self.selected_item + 1) % total;
                        }
                    }
                    Focus::Logs => {
                        // Scroll logs down
                        self.logs.disable_follow();
                        let total = self.logs.all_logs_combined().len();
                        if self.log_scroll < total.saturating_sub(1) {
                            self.log_scroll += 1;
                        }
                    }
                    _ => {}
                }
            }
            TuiStyle::Minimal => {
                let count = self.task_count();
                if count > 0 {
                    self.selected_pane = (self.selected_pane + 1) % count;
                    // Adjust page if needed
                    self.current_page = self.selected_pane / 6;
                }
            }
        }
    }

    fn navigate_up(&mut self) {
        match self.style {
            TuiStyle::Full => {
                match self.focus {
                    Focus::Groups => {
                        let total = self.groups_item_count();
                        if total > 0 {
                            self.selected_item = self
                                .selected_item
                                .checked_sub(1)
                                .unwrap_or(total.saturating_sub(1));
                        }
                    }
                    Focus::Logs => {
                        // Scroll logs up
                        self.logs.disable_follow();
                        if self.log_scroll > 0 {
                            self.log_scroll -= 1;
                        }
                    }
                    _ => {}
                }
            }
            TuiStyle::Minimal => {
                let count = self.task_count();
                if count > 0 {
                    self.selected_pane = self
                        .selected_pane
                        .checked_sub(1)
                        .unwrap_or(count.saturating_sub(1));
                    self.current_page = self.selected_pane / 6;
                }
            }
        }
    }

    fn navigate_left(&mut self) {
        // In minimal style, move to previous pane
        // In full style, same as up (for now)
        match self.style {
            TuiStyle::Minimal => self.navigate_up(),
            TuiStyle::Full => self.navigate_up(),
        }
    }

    fn navigate_right(&mut self) {
        // In minimal style, move to next pane
        // In full style, same as down (for now)
        match self.style {
            TuiStyle::Minimal => self.navigate_down(),
            TuiStyle::Full => self.navigate_down(),
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match (&self.style, &self.focus) {
            (TuiStyle::Full, Focus::Groups) => Focus::Logs,
            (TuiStyle::Full, Focus::Logs) => Focus::Groups,
            (TuiStyle::Minimal, Focus::Pane(i)) => {
                let count = self.task_count();
                if count > 0 {
                    Focus::Pane((i + 1) % count)
                } else {
                    Focus::Pane(0)
                }
            }
            _ => Focus::Groups,
        };
    }

    fn restart_selected(&mut self) {
        // Determine what's selected and send the appropriate command
        match self.selected_item_info() {
            Some(SelectedItem::Group(name)) => {
                self.send_command(ClientCommand::RestartGroup(name.clone()));
                self.set_status(format!("Restarting group {}", name));
            }
            Some(SelectedItem::Task { group, task }) => {
                self.send_command(ClientCommand::RestartProcess {
                    group: group.clone(),
                    process: task.clone(),
                });
                self.set_status(format!("Restarting task {}", task));
            }
            Some(SelectedItem::Daemon { group, daemon }) => {
                self.send_command(ClientCommand::RestartProcess {
                    group: group.clone(),
                    process: daemon.clone(),
                });
                self.set_status(format!("Restarting daemon {}", daemon));
            }
            None => {}
        }
    }

    fn restart_all(&mut self) {
        self.send_command(ClientCommand::RestartAll);
        self.set_status("Restarting all groups");
    }

    fn clear_logs(&mut self) {
        self.logs.clear_all();
        self.log_scroll = 0;
        self.set_status("Logs cleared");
    }

    fn scroll_to_top(&mut self) {
        self.log_scroll = 0;
        self.logs.disable_follow();
    }

    fn scroll_to_bottom(&mut self) {
        // This will be calculated based on actual log count during render
        self.log_scroll = usize::MAX;
        self.logs.enable_follow();
    }

    fn page_up(&mut self) {
        self.log_scroll = self.log_scroll.saturating_sub(20);
        self.logs.disable_follow();
    }

    fn page_down(&mut self) {
        self.log_scroll = self.log_scroll.saturating_add(20);
    }

    fn prev_page(&mut self) {
        if self.current_page > 0 {
            self.current_page -= 1;
            self.selected_pane = self.current_page * 6;
        }
    }

    fn next_page(&mut self) {
        let max_page = self.page_count().saturating_sub(1);
        if self.current_page < max_page {
            self.current_page += 1;
            self.selected_pane = self.current_page * 6;
        }
    }

    /// Increment animation tick.
    pub fn tick_animation(&mut self) {
        if !self.user_config.disable_animations {
            self.animation_tick = self.animation_tick.wrapping_add(1);
        }
    }

    /// Check if currently in a blinking "on" phase.
    pub fn blink_on(&self) -> bool {
        if self.user_config.disable_animations {
            true // Always show when animations disabled
        } else {
            // Blink every ~500ms (5 ticks at 100ms rate)
            (self.animation_tick / 5) % 2 == 0
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
            // Poll daemon for updates
            self.poll_daemon();

            // Tick animation
            self.tick_animation();

            // Draw UI
            terminal.draw(|frame| crate::ui::draw(frame, self))?;

            // Handle events
            if let Some(event) = events::poll(tick_rate)? {
                self.handle_event(event);
            }
        }

        Ok(())
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new(
            TuiStyle::Full,
            UserConfig::default(),
            "zaz.toml".to_string(),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_app() {
        let app = App::new(
            TuiStyle::Full,
            UserConfig::default(),
            "test.toml".to_string(),
        );
        assert_eq!(app.style, TuiStyle::Full);
        assert!(!app.should_quit);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn test_style_toggle() {
        let mut app = App::default();
        assert_eq!(app.style, TuiStyle::Full);

        app.toggle_style();
        assert_eq!(app.style, TuiStyle::Minimal);
        assert!(matches!(app.focus, Focus::Pane(0)));

        app.toggle_style();
        assert_eq!(app.style, TuiStyle::Full);
        assert_eq!(app.focus, Focus::Groups);
    }

    #[test]
    fn test_input_modes() {
        let mut app = App::default();

        app.input_mode = InputMode::Filter;
        assert_eq!(app.input_mode, InputMode::Filter);

        app.input_mode = InputMode::Search;
        assert_eq!(app.input_mode, InputMode::Search);
    }

    #[test]
    fn test_status_message() {
        let mut app = App::default();
        assert!(app.status_message.is_none());

        app.set_status("Test message");
        assert_eq!(app.status_message, Some("Test message".to_string()));

        app.clear_status();
        assert!(app.status_message.is_none());
    }

    #[test]
    fn test_blink() {
        let mut app = App::default();
        app.user_config.disable_animations = false;

        // Should blink
        app.animation_tick = 0;
        assert!(app.blink_on());

        app.animation_tick = 5;
        assert!(!app.blink_on());

        // Disable animations
        app.user_config.disable_animations = true;
        assert!(app.blink_on()); // Always on
    }

    #[test]
    fn test_page_count() {
        let app = App::default();
        assert_eq!(app.task_count(), 0);
        assert_eq!(app.page_count(), 1);
    }

    #[test]
    fn test_tui_style_from_preference() {
        assert_eq!(TuiStyle::from(TuiStylePreference::Full), TuiStyle::Full);
        assert_eq!(
            TuiStyle::from(TuiStylePreference::Minimal),
            TuiStyle::Minimal
        );
    }
}
