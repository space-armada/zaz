//! Event handling for the TUI.

use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;

/// Events that can occur in the TUI.
#[derive(Debug, Clone)]
pub enum Event {
    /// A key was pressed.
    Key(KeyEvent),

    /// Terminal was resized.
    Resize(u16, u16),

    /// Tick for periodic updates.
    Tick,
}

/// Poll for the next event.
pub fn poll(timeout: Duration) -> std::io::Result<Option<Event>> {
    if event::poll(timeout)? {
        match event::read()? {
            event::Event::Key(key) => Ok(Some(Event::Key(key))),
            event::Event::Resize(w, h) => Ok(Some(Event::Resize(w, h))),
            _ => Ok(None),
        }
    } else {
        Ok(Some(Event::Tick))
    }
}

/// Check if the event is a quit command.
pub fn is_quit(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            ..
        }) | Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}
