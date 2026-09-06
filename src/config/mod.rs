//! Configuration model, loading and validation.

pub mod duration;
pub mod interpolate;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use duration::HumanDuration;

/// Root of `omni-mcp.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub tools: ToolPolicy,
    #[serde(default)]
    pub proxies: Vec<ProxyConfig>,
    #[serde(default)]
    pub sidecars: Vec<SidecarConfig>,
    #[serde(default)]
    pub ssh: Vec<SshServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Bearer token required by the HTTP transport. Use `"${VAR}"` to source it
    /// from the environment. Absent means HTTP mode refuses to start unless
    /// `--allow-unauthenticated` is passed.
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Browser origins allowed to call the HTTP endpoint. Empty (the default)
    /// disables CORS entirely, which is correct for a local IDE gateway — the
    /// old `CorsLayer::permissive()` let any web page reach `eval_code`.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Rejects request bodies larger than this.
    #[serde(default = "default_body_limit")]
    pub max_body_bytes: usize,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
const fn default_port() -> u16 {
    8080
}
const fn default_body_limit() -> usize {
    8 * 1024 * 1024
}

impl ServerConfig {
    /// The configured bearer token, treating blank as absent.
    ///
    /// `auth_token = "${OMNI_MCP_TOKEN:-}"` with the variable unset resolves to
    /// an empty string; that must count as "no authentication configured", not
    /// as a token every request has to match exactly.
    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref().map(str::trim).filter(|token| !token.is_empty())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth_token: None,
            allowed_origins: Vec::new(),
            max_body_bytes: default_body_limit(),
        }
    }
}

/// Concurrency and deadline caps.
///
/// `max_concurrent_spawns` exists because of the incident this project was
/// built to prevent: many MCP servers launching at once through container
/// wrappers put >50 processes into uninterruptible sleep and drove load
/// average past 68. Sidecar startup is therefore serialised by default.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_tool_timeout")]
    pub default_tool_timeout: HumanDuration,
    #[serde(default = "default_max_tool_timeout")]
    pub max_tool_timeout: HumanDuration,
    #[serde(default = "default_discovery_timeout")]
    pub discovery_timeout: HumanDuration,
    #[serde(default = "default_max_concurrent_calls")]
    pub max_concurrent_calls: usize,
    #[serde(default = "default_max_concurrent_spawns")]
    pub max_concurrent_spawns: usize,
    /// How long a discovered tool list is trusted before rediscovery.
    #[serde(default = "default_discovery_ttl")]
    pub discovery_ttl: HumanDuration,
}

const fn default_tool_timeout() -> HumanDuration {
    HumanDuration::secs(30)
}
const fn default_max_tool_timeout() -> HumanDuration {
    HumanDuration::secs(300)
}
const fn default_discovery_timeout() -> HumanDuration {
    HumanDuration::millis(2_500)
}
const fn default_discovery_ttl() -> HumanDuration {
    HumanDuration::secs(60)
}
const fn default_max_concurrent_calls() -> usize {
    16
}
const fn default_max_concurrent_spawns() -> usize {
    1
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            default_tool_timeout: default_tool_timeout(),
            max_tool_timeout: default_max_tool_timeout(),
            discovery_timeout: default_discovery_timeout(),
            max_concurrent_calls: default_max_concurrent_calls(),
            max_concurrent_spawns: default_max_concurrent_spawns(),
            discovery_ttl: default_discovery_ttl(),
        }
    }
}

/// Opt-in switches for tools that can execute code or modify files.
///
/// These default to *off*. The gateway is reachable over HTTP and drives
/// arbitrary shells; enabling arbitrary code execution should be a decision the
/// operator makes explicitly, not an accident of installing the binary.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    /// Enables `eval_code` (runs arbitrary python/node/bash/... snippets).
    #[serde(default)]
    pub allow_code_execution: bool,
    /// Enables SSH and SFTP tools (`ssh_execute`, `ssh_upload`, `ssh_download`, `ssh_list_servers`).
    #[serde(default)]
    pub allow_ssh: bool,
    /// Allows `grep_search` to rewrite files and `hex_patch` to modify binaries.
    #[serde(default)]
    pub allow_file_mutation: bool,
    /// Restricts filesystem tools to these roots. Empty means unrestricted.
    #[serde(default)]
    pub allowed_roots: Vec<PathBuf>,
    /// Cap on how much of a file the filesystem tools will read.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
}

const fn default_max_file_bytes() -> u64 {
    64 * 1024 * 1024
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            allow_code_execution: false,
            allow_ssh: false,
            allow_file_mutation: false,
            allowed_roots: Vec::new(),
            max_file_bytes: default_max_file_bytes(),
        }
    }
}

/// A configured remote SSH server profile.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshServerConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub private_key: Option<PathBuf>,
    #[serde(default)]
    pub passphrase: Option<String>,
    /// Optional pinned host key fingerprint (SHA256).
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Optional SOCKS5 proxy e.g. `<socks5://127.0.0.1:1080>`.
    #[serde(default)]
    pub socks_proxy: Option<String>,
    /// Command regex whitelist (if non-empty, commands must match).
    #[serde(default)]
    pub whitelist: Vec<String>,
    /// Command regex blacklist (commands must not match).
    #[serde(default)]
    pub blacklist: Vec<String>,
    /// If true, local paths for upload/download bypass `tools.allowed_roots`.
    #[serde(default)]
    pub bypass_allowed_roots: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

const fn default_ssh_port() -> u16 {
    22
}

/// A remote MCP server reached over HTTP.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub name: String,
    pub url: String,
    /// Sent as `Authorization: Bearer <token>`.
    #[serde(default, alias = "token")]
    pub bearer: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_proxy_timeout")]
    pub timeout: HumanDuration,
    /// Prefix applied to this backend's tool names, e.g. `"ha_"`. Prevents two
    /// backends that both expose `search` from shadowing each other.
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

const fn default_proxy_timeout() -> HumanDuration {
    HumanDuration::secs(30)
}
const fn yes() -> bool {
    true
}

/// A local MCP server run as a child process speaking stdio.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Names of parent environment variables this sidecar may see.
    ///
    /// Absent means it inherits the full environment, which is the historical
    /// behaviour but rarely what you want: a desktop session commonly exports
    /// API tokens for unrelated services, and every sidecar is third-party code
    /// running with your privileges. Listing names here clears the environment
    /// and passes through only those, plus whatever `env` sets explicitly.
    #[serde(default)]
    pub inherit_env: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Spawn on first use rather than at startup. Keeping this `true` is what
    /// keeps N sidecars from launching simultaneously at IDE startup.
    #[serde(default = "yes")]
    pub lazy: bool,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout: HumanDuration,
    /// Respawn once if the process died since the last call.
    #[serde(default = "yes")]
    pub restart_on_failure: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

const fn default_startup_timeout() -> HumanDuration {
    HumanDuration::secs(20)
}

impl Config {
    /// Reads, expands `${VAR}` placeholders, parses and validates.
    ///
    /// Expansion happens on parsed string *values*, not on the raw file text,
    /// so a `${VAR}` appearing in a comment is documentation rather than a
    /// lookup that fails at startup.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let display = path.display().to_string();
        let raw = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: display.clone(), source })?;

        let parsed: toml::Value = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: display.clone(),
            source: Box::new(source),
        })?;
        let expanded = interpolate::expand_toml(parsed)?;

        let config: Self = expanded
            .try_into()
            .map_err(|source| ConfigError::Parse { path: display, source: Box::new(source) })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads `path` if it exists, otherwise returns defaults.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() { Self::load(path) } else { Ok(Self::default()) }
    }

    /// Rejects configurations that would misbehave at runtime rather than
    /// discovering the problem on the first tool call.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| Err(ConfigError::Invalid(msg));

        if self.limits.max_concurrent_calls == 0 {
            return invalid("limits.max_concurrent_calls must be at least 1".into());
        }
        if self.limits.max_concurrent_spawns == 0 {
            return invalid("limits.max_concurrent_spawns must be at least 1".into());
        }
        if self.limits.default_tool_timeout.get() > self.limits.max_tool_timeout.get() {
            return invalid(
                "limits.default_tool_timeout cannot exceed limits.max_tool_timeout".into(),
            );
        }

        let mut seen = std::collections::HashSet::new();
        for name in
            self.proxies.iter().map(|p| &p.name).chain(self.sidecars.iter().map(|s| &s.name))
        {
            if name.trim().is_empty() {
                return invalid("backend names must not be empty".into());
            }
            if !seen.insert(name) {
                return invalid(format!("duplicate backend name {name:?}"));
            }
        }

        for proxy in &self.proxies {
            if !proxy.url.starts_with("http://") && !proxy.url.starts_with("https://") {
                return invalid(format!(
                    "proxy {:?} has url {:?}; expected an http:// or https:// URL",
                    proxy.name, proxy.url
                ));
            }
        }

        for sidecar in &self.sidecars {
            if sidecar.command.trim().is_empty() {
                return invalid(format!("sidecar {:?} has an empty command", sidecar.name));
            }
        }

        let mut ssh_seen = std::collections::HashSet::new();
        for s in &self.ssh {
            if s.name.trim().is_empty() {
                return invalid("ssh server names must not be empty".into());
            }
            if !ssh_seen.insert(&s.name) {
                return invalid(format!("duplicate ssh server name {:?}", s.name));
            }
            if s.host.trim().is_empty() {
                return invalid(format!("ssh server {:?} has an empty host", s.name));
            }
            if s.user.trim().is_empty() {
                return invalid(format!("ssh server {:?} has an empty user", s.name));
            }
        }

        Ok(())
    }

    pub fn enabled_proxies(&self) -> impl Iterator<Item = &ProxyConfig> {
        self.proxies.iter().filter(|p| p.enabled)
    }

    pub fn enabled_sidecars(&self) -> impl Iterator<Item = &SidecarConfig> {
        self.sidecars.iter().filter(|s| s.enabled)
    }

    pub fn enabled_ssh(&self) -> impl Iterator<Item = &SshServerConfig> {
        self.ssh.iter().filter(|s| s.enabled)
    }
}

#[cfg(test)]
mod tests;
