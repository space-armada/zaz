//! Terminal UI for zaz.
//!
//! Provides an interactive terminal interface using Ratatui.

mod app;
mod error;
mod events;
mod ui;

pub use app::App;
pub use error::TuiError;
pub use events::Event;
