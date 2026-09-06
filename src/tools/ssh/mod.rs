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
    /// Mirrors `tools.allow_host_key_override`, so `descriptors()` (which has
    /// no `ToolContext`) does not advertise an override that would be refused.
    allow_key_override: bool,
    default_server: Option<String>,
    pool: SessionPool,
    status_cache: Arc<Mutex<HashMap<String, ServerStatus>>>,
}

impl SshTools {
    pub fn new(configs: Vec<SshServerConfig>) -> Self {
        Self::with_policy(configs, false)
    }

    pub fn with_policy(configs: Vec<SshServerConfig>, allow_key_override: bool) -> Self {
        Self::with_full_policy(configs, allow_key_override, true)
    }

    pub fn with_full_policy(
        configs: Vec<SshServerConfig>,
        allow_key_override: bool,
        allow_learning: bool,
    ) -> Self {
        let known_hosts = Arc::new(KnownHostsStore::new(KnownHostsStore::default_path()));
        let pool = SessionPool::with_learning(known_hosts, allow_learning);
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
            allow_key_override,
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

    /// Returns true if any configured server has a pending host-key fingerprint mismatch.
    fn any_mismatch_pending(&self) -> bool {
        self.pool.any_mismatch()
    }

    /// Returns true if the named server (or default) has a pending mismatch.
    fn server_has_mismatch(&self, name: Option<&str>) -> bool {
        let Some(server_name) = name.or(self.default_server.as_deref()) else {
            return false;
        };
        self.pool.has_mismatch(server_name)
    }
}

#[async_trait]
impl NativeTool for SshTools {
    fn descriptors(&self) -> Vec<Tool> {
        // Include save_new_fingerprint in schemas only when at least one server
        // has a pending mismatch. That way the agent sees it exactly when it's needed.
        let mismatch_active = self.any_mismatch_pending() && self.allow_key_override;

        let mut tools = Vec::with_capacity(4);
        tools.push(execute_descriptor(mismatch_active));
        tools.push(transfer::descriptor(mismatch_active));
        tools.push(list_descriptor());
        tools
    }

    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        ctx.require_ssh()?;

        match name {
            "ssh_execute" => self.execute_cmd(args, ctx).await,
            "ssh_transfer" => self.transfer(args, ctx).await,
            "ssh_list_servers" => self.list_servers().await,
            _ => Err(unknown(name)),
        }
    }
}

impl SshTools {
    async fn execute_cmd(&self, arguments: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        let cmd = match args::opt_string(&arguments, "cmd")? {
            Some(c) => c,
            None => args::string(&arguments, "cmdString")?,
        };

        let server_name = args::opt_string(&arguments, "server")?;

        let save_fp = self.requested_key_override(&arguments, server_name, ctx)?;

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

        let save_fp = self.requested_key_override(&arguments, server_name, ctx)?;

        let config = self.resolve_server(server_name)?;
        let session_arc = self.pool.get_or_connect(config, save_fp).await?;
        transfer::run(&arguments, ctx, config, &session_arc).await
    }

    /// Decides whether this call may overwrite a mismatched host key.
    ///
    /// Three conditions must hold: the caller asked, a mismatch is genuinely
    /// pending for that server, and the operator has enabled the override in
    /// configuration. The last is the important one — a fingerprint mismatch is
    /// either a key rotation or an active attack, and only a human can tell
    /// them apart. Letting the model set a boolean to silence the warning makes
    /// the whole check ceremonial.
    fn requested_key_override(
        &self,
        arguments: &Value,
        server_name: Option<&str>,
        ctx: &ToolContext,
    ) -> ToolResult<bool> {
        if !args::bool_or(arguments, "save_new_fingerprint", false)? {
            return Ok(false);
        }
        if !self.server_has_mismatch(server_name) {
            // Nothing to override; ignore rather than fail, since the parameter
            // may linger in a retry after the mismatch was resolved.
            return Ok(false);
        }
        ctx.require_host_key_override()?;
        Ok(true)
    }

    async fn list_servers(&self) -> ToolResult<CallToolResult> {
        let mut list = Vec::new();
        let cache = self.status_cache.lock().await;

        for config in self.servers.values() {
            let connected = self.pool.is_connected(&config.name).await;
            let status = cache.get(&config.name).cloned();
            let mismatch = self.pool.has_mismatch(&config.name);

            list.push(json!({
                "name": config.name,
                "host": config.host,
                "port": config.port,
                "username": config.user,
                "connected": connected,
                "fingerprint_mismatch_pending": mismatch,
                "status": status,
            }));
        }

        Ok(CallToolResult::structured(json!(list)))
    }
}

fn execute_descriptor(mismatch_active: bool) -> Tool {
    let mut props = json!({
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
        }
    });

    if mismatch_active {
        props["save_new_fingerprint"] = json!({
            "type": "boolean",
            "description": "A host key fingerprint mismatch was detected on a previous connection attempt. \
                            Set to true to acknowledge the new key and re-pin it as trusted. \
                            Only valid while a mismatch is pending; ignored otherwise."
        });
    }

    Tool::new(
        "ssh_execute",
        "Execute a shell command on a configured SSH server and return stdout, stderr, and exit \
         code. Access is denied unless `tools.allow_ssh = true` is set in omni-mcp.toml.",
        json!({
            "type": "object",
            "properties": props
        }),
    )
}

fn list_descriptor() -> Tool {
    Tool::new(
        "ssh_list_servers",
        "List all configured SSH servers with connection status and verbose hardware/system \
         telemetry (CPU, RAM, disk, GPU, OS, uptime, processes, services). \
         Also reports `fingerprint_mismatch_pending` per server.",
        json!({ "type": "object", "properties": {} }),
    )
}

#[cfg(test)]
mod tests;
