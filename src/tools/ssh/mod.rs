//! Native SSH and SFTP tools: remote execution, file transfer, and verbose telemetry.

pub mod known_hosts;
pub mod pool;
pub mod session;
pub mod status;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{NativeTool, ToolContext, args, unknown};
use crate::config::SshServerConfig;
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};
use known_hosts::KnownHostsStore;
use pool::SessionPool;
use status::ServerStatus;

pub struct SshTools {
    servers: HashMap<String, SshServerConfig>,
    default_server: Option<String>,
    pool: SessionPool,
    status_cache: Arc<Mutex<HashMap<String, ServerStatus>>>,
}

impl SshTools {
    pub fn new(configs: Vec<SshServerConfig>) -> Self {
        let known_hosts = Arc::new(KnownHostsStore::new(KnownHostsStore::default_path()));
        let pool = SessionPool::new(known_hosts);
        let mut servers = HashMap::new();
        let mut first = None;

        for cfg in configs {
            if cfg.enabled {
                if first.is_none() {
                    first = Some(cfg.name.clone());
                }
                servers.insert(cfg.name.clone(), cfg);
            }
        }

        Self {
            servers,
            default_server: first,
            pool,
            status_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn resolve_server<'a>(&'a self, name: Option<&str>) -> ToolResult<&'a SshServerConfig> {
        let server_name = name
            .or(self.default_server.as_deref())
            .ok_or_else(|| ToolError::InvalidArguments("no SSH servers configured".into()))?;

        self.servers.get(server_name).ok_or_else(|| {
            ToolError::NotFound(format!("configured SSH server '{server_name}' not found"))
        })
    }

    fn validate_command(config: &SshServerConfig, command: &str) -> ToolResult<()> {
        if !config.whitelist.is_empty() {
            let matched = config
                .whitelist
                .iter()
                .any(|pattern| Regex::new(pattern).is_ok_and(|re| re.is_match(command)));
            if !matched {
                return Err(ToolError::Denied(format!(
                    "command {command:?} does not match any whitelist pattern for server '{}'",
                    config.name
                )));
            }
        }

        if !config.blacklist.is_empty() {
            let matched = config
                .blacklist
                .iter()
                .any(|pattern| Regex::new(pattern).is_ok_and(|re| re.is_match(command)));
            if matched {
                return Err(ToolError::Denied(format!(
                    "command {command:?} is blocked by a blacklist pattern for server '{}'",
                    config.name
                )));
            }
        }

        Ok(())
    }

    fn validate_local_path(
        ctx: &ToolContext,
        config: &SshServerConfig,
        raw_path: &str,
    ) -> ToolResult<PathBuf> {
        if config.bypass_allowed_roots {
            let path = Path::new(raw_path);
            if path.is_relative() {
                return Err(ToolError::InvalidArguments(format!(
                    "path {raw_path:?} must be absolute"
                )));
            }
            Ok(path.to_path_buf())
        } else {
            ctx.resolve(raw_path)
        }
    }
}

#[async_trait]
impl NativeTool for SshTools {
    fn descriptors(&self) -> Vec<Tool> {
        let mut tools = Vec::with_capacity(8);
        tools.extend(execute_descriptors());
        tools.extend(transfer_descriptors());
        tools.extend(list_descriptors());
        tools
    }

    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        ctx.require_ssh()?;

        match name {
            "ssh_execute" | "execute-command" => self.execute_cmd(args).await,
            "ssh_upload" | "upload" => self.upload_file(args, ctx).await,
            "ssh_download" | "download" => self.download_file(args, ctx).await,
            "ssh_list_servers" | "list-servers" => self.list_servers().await,
            _ => Err(unknown(name)),
        }
    }
}

impl SshTools {
    async fn execute_cmd(&self, arguments: Value) -> ToolResult<CallToolResult> {
        let cmd = match args::opt_string(&arguments, "cmd")? {
            Some(c) => c,
            None => args::string(&arguments, "cmdString")?,
        };

        let server_name = match args::opt_string(&arguments, "server")? {
            Some(s) => Some(s),
            None => args::opt_string(&arguments, "connectionName")?,
        };

        let save_fp = args::bool_or(&arguments, "save_new_fingerprint", false)?;
        let config = self.resolve_server(server_name)?;
        Self::validate_command(config, cmd)?;

        let session_arc = self.pool.get_or_connect(config, save_fp).await?;
        let mut session = session_arc.lock().await;

        let (stdout, stderr, code) = session.exec(cmd).await?;

        // Background non-blocking status collection refresh
        let status_cache = Arc::clone(&self.status_cache);
        let sess_clone = Arc::clone(&session_arc);
        let s_name = config.name.clone();
        tokio::spawn(async move {
            let st = status::collect_status(&sess_clone, &s_name).await;
            status_cache.lock().await.insert(s_name, st);
        });

        let mut result = CallToolResult::structured(json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": code,
        }));
        if code != 0 {
            result.is_error = Some(true);
        }
        Ok(result)
    }

    async fn upload_file(&self, arguments: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        let local_path_str = match args::opt_string(&arguments, "local_path")? {
            Some(p) => p,
            None => args::string(&arguments, "localPath")?,
        };
        let remote_path = match args::opt_string(&arguments, "remote_path")? {
            Some(p) => p,
            None => args::string(&arguments, "remotePath")?,
        };
        let server_name = match args::opt_string(&arguments, "server")? {
            Some(s) => Some(s),
            None => args::opt_string(&arguments, "connectionName")?,
        };
        let save_fp = args::bool_or(&arguments, "save_new_fingerprint", false)?;

        let config = self.resolve_server(server_name)?;
        let local_path = Self::validate_local_path(ctx, config, local_path_str)?;

        let file_data = tokio::fs::read(&local_path).await.map_err(|e| {
            ToolError::Failed(format!("failed to read local file '{}': {e}", local_path.display()))
        })?;

        let session_arc = self.pool.get_or_connect(config, save_fp).await?;
        let mut session = session_arc.lock().await;
        let sftp = session.get_sftp().await?;

        sftp.write(remote_path, &file_data).await.map_err(|e| {
            ToolError::Failed(format!("failed to write remote file '{remote_path}': {e}"))
        })?;

        Ok(CallToolResult::text("File uploaded successfully"))
    }

    async fn download_file(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> ToolResult<CallToolResult> {
        ctx.require_file_mutation()?;

        let remote_path = match args::opt_string(&arguments, "remote_path")? {
            Some(p) => p,
            None => args::string(&arguments, "remotePath")?,
        };
        let local_path_str = match args::opt_string(&arguments, "local_path")? {
            Some(p) => p,
            None => args::string(&arguments, "localPath")?,
        };
        let server_name = match args::opt_string(&arguments, "server")? {
            Some(s) => Some(s),
            None => args::opt_string(&arguments, "connectionName")?,
        };
        let save_fp = args::bool_or(&arguments, "save_new_fingerprint", false)?;

        let config = self.resolve_server(server_name)?;
        let local_path = Self::validate_local_path(ctx, config, local_path_str)?;

        let session_arc = self.pool.get_or_connect(config, save_fp).await?;
        let mut session = session_arc.lock().await;
        let sftp = session.get_sftp().await?;

        let data = sftp.read(remote_path).await.map_err(|e| {
            ToolError::Failed(format!("failed to read remote file '{remote_path}': {e}"))
        })?;

        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::Failed(format!("failed to create local parent directory: {e}"))
            })?;
        }

        tokio::fs::write(&local_path, data).await.map_err(|e| {
            ToolError::Failed(format!("failed to write local file '{}': {e}", local_path.display()))
        })?;

        Ok(CallToolResult::text("File downloaded successfully"))
    }

    async fn list_servers(&self) -> ToolResult<CallToolResult> {
        let mut list = Vec::new();
        let cache = self.status_cache.lock().await;

        for config in self.servers.values() {
            let connected = self.pool.is_connected(&config.name).await;
            let status = cache.get(&config.name).cloned();

            list.push(json!({
                "name": config.name,
                "host": config.host,
                "port": config.port,
                "username": config.user,
                "connected": connected,
                "status": status,
            }));
        }

        Ok(CallToolResult::structured(json!(list)))
    }
}

fn execute_descriptors() -> Vec<Tool> {
    vec![
        Tool::new(
            "ssh_execute",
            "Execute command on connected SSH server and get stdout/stderr results. \
             Access is denied unless `tools.allow_ssh = true` is configured in omni-mcp.toml.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Command to execute on the remote host" },
                    "cmdString": { "type": "string", "description": "Alias for cmd" },
                    "server": { "type": "string", "description": "Server profile name (optional, default is first configured server)" },
                    "connectionName": { "type": "string", "description": "Alias for server" },
                    "timeout": { "type": "integer", "description": "Execution timeout in seconds or milliseconds" },
                    "save_new_fingerprint": { "type": "boolean", "description": "Set to true to acknowledge and re-pin host key fingerprint on mismatch" }
                }
            }),
        ),
        Tool::new(
            "execute-command",
            "Execute command on connected server and get output result (compatibility alias for ssh_execute).",
            json!({
                "type": "object",
                "properties": {
                    "cmdString": { "type": "string", "description": "Command to execute" },
                    "cmd": { "type": "string", "description": "Alias for cmdString" },
                    "connectionName": { "type": "string", "description": "SSH connection name (optional)" },
                    "server": { "type": "string", "description": "Alias for connectionName" },
                    "timeout": { "type": "integer", "description": "Command execution timeout in milliseconds or seconds" },
                    "save_new_fingerprint": { "type": "boolean", "description": "Set to true to acknowledge and re-pin host key fingerprint on mismatch" }
                }
            }),
        ),
    ]
}

fn transfer_descriptors() -> Vec<Tool> {
    vec![
        Tool::new(
            "ssh_upload",
            "Upload a local file to a remote path on the connected SSH server via SFTP. \
             Access is denied unless `tools.allow_ssh = true` is configured in omni-mcp.toml.",
            json!({
                "type": "object",
                "properties": {
                    "local_path": { "type": "string", "description": "Path to local file to upload" },
                    "localPath": { "type": "string", "description": "Alias for local_path" },
                    "remote_path": { "type": "string", "description": "Destination path on the remote server" },
                    "remotePath": { "type": "string", "description": "Alias for remote_path" },
                    "server": { "type": "string", "description": "Server profile name (optional)" },
                    "connectionName": { "type": "string", "description": "Alias for server" },
                    "save_new_fingerprint": { "type": "boolean", "description": "Set to true to acknowledge and re-pin host key fingerprint on mismatch" }
                }
            }),
        ),
        Tool::new(
            "upload",
            "Upload file to connected server (compatibility alias for ssh_upload).",
            json!({
                "type": "object",
                "properties": {
                    "localPath": { "type": "string", "description": "Local path" },
                    "local_path": { "type": "string", "description": "Alias for localPath" },
                    "remotePath": { "type": "string", "description": "Remote path" },
                    "remote_path": { "type": "string", "description": "Alias for remotePath" },
                    "connectionName": { "type": "string", "description": "SSH connection name (optional)" },
                    "server": { "type": "string", "description": "Alias for connectionName" },
                    "save_new_fingerprint": { "type": "boolean", "description": "Set to true to acknowledge and re-pin host key fingerprint on mismatch" }
                }
            }),
        ),
        Tool::new(
            "ssh_download",
            "Download a remote file to a local path from the connected SSH server via SFTP. \
             Access is denied unless `tools.allow_ssh = true` is configured in omni-mcp.toml.",
            json!({
                "type": "object",
                "properties": {
                    "remote_path": { "type": "string", "description": "Remote path to download from" },
                    "remotePath": { "type": "string", "description": "Alias for remote_path" },
                    "local_path": { "type": "string", "description": "Local destination path" },
                    "localPath": { "type": "string", "description": "Alias for local_path" },
                    "server": { "type": "string", "description": "Server profile name (optional)" },
                    "connectionName": { "type": "string", "description": "Alias for server" },
                    "save_new_fingerprint": { "type": "boolean", "description": "Set to true to acknowledge and re-pin host key fingerprint on mismatch" }
                }
            }),
        ),
        Tool::new(
            "download",
            "Download file from connected server (compatibility alias for ssh_download).",
            json!({
                "type": "object",
                "properties": {
                    "remotePath": { "type": "string", "description": "Remote path" },
                    "remote_path": { "type": "string", "description": "Alias for remotePath" },
                    "localPath": { "type": "string", "description": "Local path" },
                    "local_path": { "type": "string", "description": "Alias for localPath" },
                    "connectionName": { "type": "string", "description": "SSH connection name (optional)" },
                    "server": { "type": "string", "description": "Alias for connectionName" },
                    "save_new_fingerprint": { "type": "boolean", "description": "Set to true to acknowledge and re-pin host key fingerprint on mismatch" }
                }
            }),
        ),
    ]
}

fn list_descriptors() -> Vec<Tool> {
    vec![
        Tool::new(
            "ssh_list_servers",
            "List all configured SSH servers along with active connection status and verbose hardware/system telemetry.",
            json!({ "type": "object", "properties": {} }),
        ),
        Tool::new(
            "list-servers",
            "List all available SSH server configurations (compatibility alias for ssh_list_servers).",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}
