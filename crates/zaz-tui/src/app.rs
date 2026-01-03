//! Application state and logic.

use crate::daemon::{ClientCommand, DaemonConnection};
use crate::logs::LogBuffer;
use crate::styles::{get_renderer, KeyResult};
use crate::{events, Event, TuiError};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::collections::HashMap;
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
    /// Log scroll offset (first visible line) for Full style.
    pub log_scroll: usize,
    /// Visible height of log pane (updated during render) for Full style.
    pub log_visible_height: usize,
    /// Per-pane scroll offsets (for Minimal style).
    pub pane_scroll: HashMap<usize, usize>,
    /// Per-pane follow mode (for Minimal style).
    pub pane_follow: HashMap<usize, bool>,
    /// Per-pane visible height (for Minimal style, updated during render).
    pub pane_visible_height: HashMap<usize, usize>,
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
    /// Transient status message with expiration.
    pub transient_message: Option<TransientMessage>,
    /// Whether to show help overlay.
    pub show_help: bool,
}

/// A transient status message that auto-expires.
#[derive(Debug, Clone)]
pub struct TransientMessage {
    /// The message text.
    pub text: String,
    /// When the message was created (for expiration).
    pub created_at: std::time::Instant,
    /// Whether this is an error message (longer display time).
    pub is_error: bool,
}

impl TransientMessage {
    /// Duration before success messages expire.
    const SUCCESS_DURATION: std::time::Duration = std::time::Duration::from_secs(3);
    /// Duration before error messages expire.
    const ERROR_DURATION: std::time::Duration = std::time::Duration::from_secs(8);

    /// Create a new success message.
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            created_at: std::time::Instant::now(),
            is_error: false,
        }
    }

    /// Create a new error message.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            created_at: std::time::Instant::now(),
            is_error: true,
        }
    }

    /// Check if the message has expired.
    pub fn is_expired(&self) -> bool {
        let duration = if self.is_error {
            Self::ERROR_DURATION
        } else {
            Self::SUCCESS_DURATION
        };
        self.created_at.elapsed() > duration
    }
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
            log_visible_height: 20, // Default, updated during render
            pane_scroll: HashMap::new(),
            pane_follow: HashMap::new(),
            pane_visible_height: HashMap::new(),
            show_full_timestamp: false,
            input_mode: InputMode::Normal,
            filter_input: String::new(),
            search_input: String::new(),
            animation_tick: 0,
            user_config,
            started_daemon: false,
            should_quit: false,
            transient_message: None,
            show_help: false,
        }
    }

    /// Get scroll offset for a specific pane.
    pub fn get_pane_scroll(&self, pane: usize) -> usize {
        self.pane_scroll.get(&pane).copied().unwrap_or(0)
    }

    /// Set scroll offset for a specific pane.
    pub fn set_pane_scroll(&mut self, pane: usize, offset: usize) {
        self.pane_scroll.insert(pane, offset);
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
            count.div_ceil(6) // Ceiling division
        }
    }

    /// Set a transient status message (auto-expires after a few seconds).
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.transient_message = Some(TransientMessage::success(message));
    }

    /// Set a transient error message (auto-expires after longer delay).
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.transient_message = Some(TransientMessage::error(message));
    }

    /// Get the active transient message (if not expired).
    pub fn active_transient_message(&self) -> Option<&TransientMessage> {
        self.transient_message.as_ref().filter(|m| !m.is_expired())
    }

    /// Clear the transient message.
    pub fn clear_status(&mut self) {
        self.transient_message = None;
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
            // Handle global keys first (style switching, help, modes)
            match key.code {
                // Style switching
                KeyCode::F(1) => {
                    self.style = TuiStyle::Full;
                    let renderer = get_renderer(self.style);
                    renderer.on_activate(self);
                    return;
                }
                KeyCode::F(2) => {
                    self.style = TuiStyle::Minimal;
                    let renderer = get_renderer(self.style);
                    renderer.on_activate(self);
                    return;
                }

                // Help toggle
                KeyCode::Char('?') => {
                    self.show_help = true;
                    return;
                }

                // Input modes (filter/search)
                KeyCode::Char('f') => {
                    self.input_mode = InputMode::Filter;
                    self.filter_input.clear();
                    return;
                }
                KeyCode::Char('/') => {
                    self.input_mode = InputMode::Search;
                    self.search_input.clear();
                    return;
                }

                // Search navigation
                KeyCode::Char('n') => {
                    if let Some(line) = self.logs.next_search_match() {
                        self.log_scroll = line;
                    }
                    return;
                }
                KeyCode::Char('N') => {
                    if let Some(line) = self.logs.prev_search_match() {
                        self.log_scroll = line;
                    }
                    return;
                }

                // Clear filter/search
                KeyCode::Esc => {
                    self.logs.clear_filter();
                    self.logs.clear_search();
                    self.clear_status();
                    return;
                }

                _ => {}
            }

            // Delegate to style-specific handler
            let renderer = get_renderer(self.style);
            let result = renderer.handle_key(self, key.code);

            // Handle the result
            match result {
                KeyResult::Handled => {}
                KeyResult::NotHandled => {}
                KeyResult::Restart(selected) => {
                    if selected.is_group {
                        self.send_command(ClientCommand::RestartGroup(selected.group.clone()));
                        self.set_status(format!("Restarting group {}", selected.group));
                    } else {
                        self.send_command(ClientCommand::RestartProcess {
                            group: selected.group.clone(),
                            process: selected.process.clone(),
                        });
                        self.set_status(format!("Restarting {}", selected.process));
                    }
                }
                KeyResult::RestartAll => {
                    self.send_command(ClientCommand::RestartAll);
                    self.set_status("Restarting all groups");
                }
                KeyResult::Command(cmd) => {
                    self.send_command(cmd);
                }
                KeyResult::SetStatus(msg) => {
                    self.set_status(msg);
                }
                KeyResult::SetError(msg) => {
                    self.set_error(msg);
                }
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
    fn test_transient_message() {
        let mut app = App::default();
        assert!(app.transient_message.is_none());

        app.set_status("Test message");
        assert!(app.transient_message.is_some());
        assert_eq!(app.transient_message.as_ref().unwrap().text, "Test message");
        assert!(!app.transient_message.as_ref().unwrap().is_error);

        app.set_error("Error message");
        assert!(app.transient_message.as_ref().unwrap().is_error);

        app.clear_status();
        assert!(app.transient_message.is_none());
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
