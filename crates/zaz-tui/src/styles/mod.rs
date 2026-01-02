//! Extensible style system for the TUI.
//!
//! This module provides a trait-based architecture for rendering different
//! TUI styles. New styles can be added by implementing the `StyleRenderer` trait.

mod full;
mod minimal;

pub use full::FullStyle;
pub use minimal::MinimalStyle;

use crate::app::App;
use crossterm::event::KeyCode;
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

/// Trait for rendering a TUI style.
///
/// Implement this trait to add a new display style to the TUI.
/// The trait provides methods for drawing, layout calculation, and navigation.
pub trait StyleRenderer: Send + Sync {
    /// Draw the complete UI for this style.
    fn draw(&self, frame: &mut Frame, app: &App);

    /// Calculate the layout of panes for the given area and task count.
    ///
    /// Returns a list of `PaneLayout` structs describing each pane's position.
    fn calculate_layout(&self, area: Rect, task_count: usize) -> Vec<PaneLayout>;

    /// Handle style-specific navigation based on key input.
    ///
    /// This is called for navigation keys (arrows, j/k, etc.) to allow
    /// style-specific behavior like tree navigation in Full style or
    /// pane switching in Minimal style.
    fn handle_navigation(&self, app: &mut App, key: KeyCode);

    /// Get the display name of this style.
    fn name(&self) -> &'static str;
}

/// Get a renderer for the specified style.
pub fn get_renderer(style: crate::app::TuiStyle) -> Box<dyn StyleRenderer> {
    match style {
        crate::app::TuiStyle::Full => Box::new(FullStyle),
        crate::app::TuiStyle::Minimal => Box::new(MinimalStyle),
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
    fn test_get_renderer_minimal() {
        let renderer = get_renderer(TuiStyle::Minimal);
        assert_eq!(renderer.name(), "Minimal");
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
}
