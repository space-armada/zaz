//! Stdio-transport MCP server for zaz.
//!
//! Tool registration arrives in subsequent milestones; this scaffold implements
//! `ServerHandler::get_info` so the MCP `initialize` handshake completes.

use rmcp::handler::server::ServerHandler;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::ServiceExt;

use crate::error::McpError;

/// MCP server handler for zaz.
#[derive(Clone, Default)]
pub struct ZazMcpServer;

impl ZazMcpServer {
    /// Create a new server instance.
    pub fn new() -> Self {
        Self
    }
}

impl ServerHandler for ZazMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_instructions("zaz MCP tool server".to_string())
    }
}

/// Run the MCP server over stdio until the peer disconnects.
pub async fn run() -> Result<(), McpError> {
    let service = ZazMcpServer::new()
        .serve(stdio())
        .await
        .map_err(|e| McpError::Serve(e.to_string()))?;
    service
        .waiting()
        .await
        .map_err(|e| McpError::Serve(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_info_advertises_zaz_mcp() {
        let info = ZazMcpServer::new().get_info();
        assert_eq!(info.server_info.name, "zaz-mcp");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.instructions.as_deref(), Some("zaz MCP tool server"));
    }

    #[test]
    fn get_info_enables_tool_capability() {
        let info = ZazMcpServer::new().get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability must be advertised"
        );
    }
}
