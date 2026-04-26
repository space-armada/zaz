//! Extensible style system for the TUI.
//!
//! This module provides a trait-based architecture for rendering different
//! TUI styles. New styles can be added by implementing the `StyleRenderer` trait.
//!
//! # Architecture
//!
//! Each style is responsible for:
//! - Drawing the UI (`draw`)
//! - Handling all keyboard input (`handle_key`)
//! - Managing its own state (scroll positions, selection, etc.)
//!
//! The `App` struct delegates input handling to the active style via the
//! `handle_key` method, allowing each style to have completely different
//! behavior for the same keys.

mod full;
mod multi_pane;

pub use full::FullStyle;
pub use multi_pane::MultiPaneStyle;

use crate::app::App;
use crate::daemon::ClientCommand;
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

/// Layout information for a single pane in the TUI.
#[derive(Debug, Clone)]
pub struct PaneLayout {
    /// The rectangular area for this pane.
    pub area: Rect,
    /// The process name associated with this pane (if any).
    pub process: Option<String>,
    /// Whether this pane is currently focused.
    pub focused: bool,
}

/// Information about the currently selected process.
#[derive(Debug, Clone)]
pub struct SelectedProcess {
    /// The group name containing this process.
    pub group: String,
    /// The process name.
    pub process: String,
    /// Whether this is a group (vs individual task/daemon).
    pub is_group: bool,
}

/// Result of handling a key event.
#[derive(Debug, Clone)]
pub enum KeyResult {
    /// Key was handled, no further action needed.
    Handled,
    /// Key was not handled, try default handlers.
    NotHandled,
    /// Request to restart the selected process.
    Restart(SelectedProcess),
    /// Request to restart all groups.
    RestartAll,
    /// Request to send a command to the daemon.
    Command(ClientCommand),
    /// Request to set a status message.
    SetStatus(String),
    /// Request to set an error message.
    SetError(String),
}

/// Trait for rendering a TUI style.
///
/// Implement this trait to add a new display style to the TUI.
/// Each style is responsible for:
/// - Drawing the complete UI
/// - Handling all keyboard input (navigation, actions, etc.)
/// - Managing scroll state for its panes
///
/// # Required Methods
///
/// - `draw`: Render the UI for this style
/// - `handle_key`: Process keyboard input
/// - `name`: Return the style name for display
///
/// # State Management
///
/// Styles should use `App` fields for state:
/// - `selected_item`: Selection index for Full style's groups tree
/// - `selected_pane`: Selection index for Multi Pane style's panes
/// - `pane_scroll`: Per-pane scroll positions for Multi Pane style
/// - `log_scroll`: Global log scroll for Full style
/// - `focus`: Which element is focused
pub trait StyleRenderer: Send + Sync {
    /// Draw the complete UI for this style.
    ///
    /// This method should render all UI elements including:
    /// - Main content area (groups tree, panes, logs)
    /// - Status bar
    /// - Any style-specific overlays
    fn draw(&self, frame: &mut Frame, app: &mut App);

    /// Handle a key press.
    ///
    /// This is the main entry point for all keyboard input when this style
    /// is active. The style should handle:
    /// - Navigation (j/k, arrows, Tab, etc.)
    /// - Actions (r for restart, c for clear, etc.)
    /// - Scrolling (g/G, PgUp/PgDn, Ctrl+d/u)
    /// - Mode switching (handled by App, but style can request it)
    ///
    /// Returns `KeyResult` indicating what happened:
    /// - `Handled`: Key was processed, no further action
    /// - `NotHandled`: Key was not recognized, try default handlers
    /// - `Restart(process)`: Request to restart a process
    /// - `Command(cmd)`: Send a command to the daemon
    fn handle_key(&self, app: &mut App, key: KeyEvent) -> KeyResult;

    /// Get the display name of this style.
    fn name(&self) -> &'static str;

    /// Calculate the layout of panes for the given area and task count.
    ///
    /// Returns a list of `PaneLayout` structs describing each pane's position.
    /// This is primarily used for testing and introspection.
    fn calculate_layout(&self, area: Rect, task_count: usize) -> Vec<PaneLayout>;

    /// Get information about the currently selected process.
    ///
    /// Returns `None` if nothing is selected or the selection is invalid.
    fn get_selected_process(&self, app: &App) -> Option<SelectedProcess>;

    /// Called when the style becomes active.
    ///
    /// Use this to initialize style-specific state.
    fn on_activate(&self, _app: &mut App) {}

    /// Get the total number of log lines visible in the current view.
    ///
    /// Used for calculating scroll limits. Returns (visible_height, total_lines).
    fn log_dimensions(&self, app: &App) -> (usize, usize) {
        let combined = app.logs.all_logs_combined();
        (20, combined.len()) // Default: 20 visible lines
    }
}

/// Get a renderer for the specified style.
pub fn get_renderer(style: crate::app::TuiStyle) -> Box<dyn StyleRenderer> {
    match style {
        crate::app::TuiStyle::Full => Box::new(FullStyle),
        crate::app::TuiStyle::MultiPane => Box::new(MultiPaneStyle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TuiStyle;

    #[test]
    fn test_get_renderer_full() {
        let renderer = get_renderer(TuiStyle::Full);
        assert_eq!(renderer.name(), "Full");
    }

    #[test]
    fn test_get_renderer_multi_pane() {
        let renderer = get_renderer(TuiStyle::MultiPane);
        assert_eq!(renderer.name(), "Multi Pane");
    }

    #[test]
    fn test_pane_layout() {
        let layout = PaneLayout {
            area: Rect::new(0, 0, 80, 24),
            process: Some("server".to_string()),
            focused: true,
        };
        assert!(layout.focused);
        assert_eq!(layout.process, Some("server".to_string()));
    }

    #[test]
    fn test_selected_process() {
        let selected = SelectedProcess {
            group: "backend".to_string(),
            process: "server".to_string(),
            is_group: false,
        };
        assert_eq!(selected.group, "backend");
        assert_eq!(selected.process, "server");
        assert!(!selected.is_group);
    }
}
