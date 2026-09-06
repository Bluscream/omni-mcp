//! Native SSH and SFTP tools: remote execution, file transfer, and verbose telemetry.

pub mod known_hosts;
pub mod pool;
pub mod session;
pub mod status;
pub mod transfer;

use std::collections::HashMap;
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
}

#[async_trait]
impl NativeTool for SshTools {
    fn descriptors(&self) -> Vec<Tool> {
        let mut tools = Vec::with_capacity(4);
        tools.push(execute_descriptor());
        tools.push(transfer::descriptor());
        tools.push(list_descriptor());
        tools
    }

    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        ctx.require_ssh()?;

        match name {
            "ssh_execute" => self.execute_cmd(args).await,
            "ssh_transfer" => self.transfer(args, ctx).await,
            "ssh_list_servers" => self.list_servers().await,
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

        let server_name = args::opt_string(&arguments, "server")?;
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

    async fn transfer(&self, arguments: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        let server_name = args::opt_string(&arguments, "server")?;
        let save_fp = args::bool_or(&arguments, "save_new_fingerprint", false)?;
        let config = self.resolve_server(server_name)?;

        let session_arc = self.pool.get_or_connect(config, save_fp).await?;
        transfer::run(&arguments, ctx, config, &session_arc).await
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

fn execute_descriptor() -> Tool {
    Tool::new(
        "ssh_execute",
        "Execute a shell command on a configured SSH server and return stdout, stderr, and exit \
         code. Access is denied unless `tools.allow_ssh = true` is set in omni-mcp.toml.",
        json!({
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "Command to execute on the remote host"
                },
                "cmdString": {
                    "type": "string",
                    "description": "Alias for cmd (legacy compatibility)"
                },
                "server": {
                    "type": "string",
                    "description": "Server profile name from omni-mcp.toml (optional; defaults to first configured server)"
                },
                "save_new_fingerprint": {
                    "type": "boolean",
                    "description": "Set to true to acknowledge and re-pin the host key when a fingerprint mismatch is detected"
                }
            }
        }),
    )
}

fn list_descriptor() -> Tool {
    Tool::new(
        "ssh_list_servers",
        "List all configured SSH servers with connection status and verbose hardware/system \
         telemetry (CPU, RAM, disk, GPU, OS, uptime, processes, services).",
        json!({ "type": "object", "properties": {} }),
    )
}
