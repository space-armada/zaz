//! Variable expansion for zaz commands.
//!
//! Supports `${var}` syntax with escaping via `\${var}`.

mod error;
mod expand;

pub use error::VarError;
pub use expand::{Context, Expander};
