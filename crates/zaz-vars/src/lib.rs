//! Variable expansion for zaz commands.
//!
//! Supports `${var}` syntax with escaping via `\${var}`.

mod error;
mod expand;
mod scan;

pub use error::VarError;
pub use expand::{Context, Expander};
pub use scan::{references, FILE_CONTEXT_BUILTINS};
