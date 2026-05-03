//! MCP tool server for zaz.
//!
//! Exposes a stdio-transport MCP server that AI assistants can use to query
//! daemon state and trigger control operations.

mod error;
mod server;

pub use error::McpError;
pub use server::{run, ZazMcpServer};
