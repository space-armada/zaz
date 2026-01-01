//! Error types for variable expansion.

use thiserror::Error;

/// Errors that can occur during variable expansion.
#[derive(Debug, Error)]
pub enum VarError {
    /// Referenced an undefined variable.
    #[error("undefined variable: ${{{0}}}")]
    Undefined(String),

    /// Malformed variable syntax.
    #[error("malformed variable syntax at position {0}")]
    Malformed(usize),
}
