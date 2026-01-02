//! Terminal UI for zaz.
//!
//! Provides an interactive terminal interface using Ratatui.

mod app;
pub mod daemon;
mod error;
mod events;
pub mod help;
pub mod logs;
pub mod styles;
mod ui;

pub use app::{App, Focus, InputMode, TuiStyle};
pub use daemon::{ClientCommand, DaemonConnection, LogLine, LogSource};
pub use error::TuiError;
pub use events::Event;
pub use help::draw_help;
pub use logs::{LogBuffer, StoredLog};
pub use styles::{get_renderer, FullStyle, MinimalStyle, PaneLayout, StyleRenderer};
