//! Stdio-transport MCP server for zaz, with read-only diagnostic tools.
//!
//! Tools dispatch to the running daemon via the Unix socket API, except for
//! `zaz_config` which loads the project config directly from disk. Mutation
//! tools and CLI flag overrides are added in subsequent milestones.

use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};

use crate::client;
use crate::error::McpError;
use crate::types::{ConfigReport, GroupsReport, LogsReport, LogsRequest, StatusReport};

/// MCP server handler for zaz.
#[derive(Clone)]
pub struct ZazMcpServer {
    cwd: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl ZazMcpServer {
    /// Create a server rooted at `cwd`, used for socket and config discovery.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl ZazMcpServer {
    /// Return the current state of the zaz daemon: groups, processes, watched files.
    #[tool(
        name = "zaz_status",
        description = "Get the current state of the zaz daemon, including all groups, their processes (tasks and daemons), PIDs, and recent file-change activity. Use this to answer 'is the daemon running?' and 'is process X up?'."
    )]
    async fn zaz_status(&self) -> Result<Json<StatusReport>, ErrorData> {
        let state = client::fetch_status(&self.cwd).await.map_err(into_error)?;
        Ok(Json(StatusReport::from(&state)))
    }

    /// Slim listing of groups and their high-level status.
    #[tool(
        name = "zaz_list_groups",
        description = "List the configured groups and their high-level status. Lighter than zaz_status; use this when you only need to know which groups exist and whether they are running."
    )]
    async fn zaz_list_groups(&self) -> Result<Json<GroupsReport>, ErrorData> {
        let state = client::fetch_status(&self.cwd).await.map_err(into_error)?;
        Ok(Json(GroupsReport::from(&state)))
    }

    /// Paginated, optionally-filtered log query.
    #[tool(
        name = "zaz_logs",
        description = "Read captured log output for a process. `name` is the process name (e.g. \"server\"); use \"*\" or omit to query across all processes. Supports pagination (`offset`, `limit`) and case-insensitive substring search."
    )]
    async fn zaz_logs(
        &self,
        Parameters(req): Parameters<LogsRequest>,
    ) -> Result<Json<LogsReport>, ErrorData> {
        let page = client::fetch_logs(&self.cwd, &req)
            .await
            .map_err(into_error)?;
        Ok(Json(LogsReport {
            name: page.name,
            entries: page.lines.iter().map(Into::into).collect(),
            total_count: page.total_count,
            has_more: page.has_more,
            offset: page.offset,
        }))
    }

    /// Return the parsed project configuration from disk.
    #[tool(
        name = "zaz_config",
        description = "Return the parsed zaz project configuration: groups, file patterns, task and daemon commands, and global settings. Use this to understand how the project is wired up before diagnosing why something isn't restarting."
    )]
    async fn zaz_config(&self) -> Result<Json<ConfigReport>, ErrorData> {
        let cwd = self.cwd.clone();
        let (path, config) = tokio::task::spawn_blocking(move || client::discover_config(&cwd))
            .await
            .map_err(|e| ErrorData::internal_error(format!("config join error: {e}"), None))?
            .map_err(into_error)?;
        Ok(Json(ConfigReport::from_config(&path, &config)))
    }
}

#[tool_handler(router = self.tool_router)]
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
    let cwd = std::env::current_dir()?;
    let service = ZazMcpServer::new(cwd)
        .serve(stdio())
        .await
        .map_err(|e| McpError::Serve(e.to_string()))?;
    service
        .waiting()
        .await
        .map_err(|e| McpError::Serve(e.to_string()))?;
    Ok(())
}

fn into_error(err: McpError) -> ErrorData {
    ErrorData::internal_error(err.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> ZazMcpServer {
        ZazMcpServer::new(PathBuf::from("."))
    }

    #[test]
    fn get_info_advertises_zaz_mcp() {
        let info = server().get_info();
        assert_eq!(info.server_info.name, "zaz-mcp");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.instructions.as_deref(), Some("zaz MCP tool server"));
    }

    #[test]
    fn get_info_enables_tool_capability() {
        let info = server().get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability must be advertised"
        );
    }

    #[test]
    fn tool_router_lists_all_read_tools() {
        let router = ZazMcpServer::tool_router();
        let tools = router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            names.contains(&"zaz_status"),
            "missing zaz_status in {names:?}"
        );
        assert!(
            names.contains(&"zaz_list_groups"),
            "missing zaz_list_groups in {names:?}"
        );
        assert!(names.contains(&"zaz_logs"), "missing zaz_logs in {names:?}");
        assert!(
            names.contains(&"zaz_config"),
            "missing zaz_config in {names:?}"
        );
        assert_eq!(names.len(), 4, "expected exactly four tools, got {names:?}");
    }
}
