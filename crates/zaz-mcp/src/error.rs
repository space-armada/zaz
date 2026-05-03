//! Error types for the MCP server.

use thiserror::Error;

/// Errors that can occur while running the MCP server.
#[derive(Debug, Error)]
pub enum McpError {
    /// I/O failure on the stdio transport.
    #[error("mcp transport error: {0}")]
    Io(#[from] std::io::Error),

    /// Failure raised by the rmcp service layer (initialize, dispatch, shutdown).
    #[error("mcp service error: {0}")]
    Serve(String),
}
