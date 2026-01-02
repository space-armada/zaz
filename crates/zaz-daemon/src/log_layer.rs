//! Custom tracing layer for capturing daemon internal logs.
//!
//! This layer captures tracing events and forwards them to the Engine's log
//! storage so they appear in the TUI alongside process output.

use crate::api::LogLine;
use std::fmt::Write;
use tokio::sync::mpsc;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// A tracing layer that forwards events to the daemon log system.
pub struct DaemonLogLayer {
    /// Sender for log lines to the Engine.
    log_tx: mpsc::Sender<LogLine>,
}

impl DaemonLogLayer {
    /// Create a new daemon log layer.
    pub fn new(log_tx: mpsc::Sender<LogLine>) -> Self {
        Self { log_tx }
    }
}

impl<S: Subscriber> Layer<S> for DaemonLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Extract fields from the event
        let mut message = String::new();
        let mut process = None;
        let mut group = None;

        // Visit the event fields
        event.record(&mut FieldVisitor {
            message: &mut message,
            process: &mut process,
            group: &mut group,
        });

        // If no message was extracted, skip
        if message.is_empty() {
            return;
        }

        // Use "daemon" as the default process if none specified
        let process_name = process.unwrap_or_else(|| "daemon".to_string());

        // Create log line
        let mut log_line = LogLine::daemon(&process_name, &message);
        if let Some(g) = group {
            log_line = log_line.with_group(g);
        }

        // Send to engine (non-blocking)
        let _ = self.log_tx.try_send(log_line);
    }
}

/// Visitor to extract fields from tracing events.
struct FieldVisitor<'a> {
    message: &'a mut String,
    process: &'a mut Option<String>,
    group: &'a mut Option<String>,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => {
                let _ = write!(self.message, "{:?}", value);
                // Remove surrounding quotes if present
                if self.message.starts_with('"') && self.message.ends_with('"') {
                    let trimmed = self.message[1..self.message.len() - 1].to_string();
                    *self.message = trimmed;
                }
            }
            "daemon" | "task" | "name" | "process" => {
                *self.process = Some(format!("{:?}", value).trim_matches('"').to_string());
            }
            "group" => {
                *self.group = Some(format!("{:?}", value).trim_matches('"').to_string());
            }
            _ => {
                // Append other fields to message
                if !self.message.is_empty() {
                    self.message.push_str(", ");
                }
                let _ = write!(self.message, "{}={:?}", field.name(), value);
            }
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => {
                *self.message = value.to_string();
            }
            "daemon" | "task" | "name" | "process" => {
                *self.process = Some(value.to_string());
            }
            "group" => {
                *self.group = Some(value.to_string());
            }
            _ => {
                if !self.message.is_empty() {
                    self.message.push_str(", ");
                }
                let _ = write!(self.message, "{}={}", field.name(), value);
            }
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if !self.message.is_empty() {
            self.message.push_str(", ");
        }
        let _ = write!(self.message, "{}={}", field.name(), value);
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if !self.message.is_empty() {
            self.message.push_str(", ");
        }
        let _ = write!(self.message, "{}={}", field.name(), value);
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if !self.message.is_empty() {
            self.message.push_str(", ");
        }
        let _ = write!(self.message, "{}={}", field.name(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_log_layer_creation() {
        let (tx, _rx) = mpsc::channel(16);
        let _layer = DaemonLogLayer::new(tx);
    }
}
