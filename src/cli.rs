//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Consolidated MCP gateway: native Rust tools, supervised sidecars and HTTP
/// proxies behind a single process.
#[derive(Debug, Parser)]
#[command(name = "omni-mcp", version, about, long_about = None)]
pub struct Cli {
    /// Path to omni-mcp.toml.
    #[arg(long, short, env = "OMNI_MCP_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    /// Log verbosity. Overrides `RUST_LOG`.
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log: Option<String>,

    /// Deprecated alias for the `stdio` subcommand, kept so existing IDE
    /// configurations (`"args": ["--stdio"]`) keep working.
    #[arg(long, hide = true)]
    pub stdio: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve MCP over stdin/stdout. This is what an IDE launches. (Default.)
    Stdio,

    /// Serve MCP over HTTP.
    Serve {
        /// Address to bind. Overrides server.host and server.port.
        #[arg(long)]
        bind: Option<String>,

        /// Start without a bearer token. Every tool, including code execution,
        /// becomes reachable by any local process.
        #[arg(long)]
        allow_unauthenticated: bool,
    },

    /// Load the configuration, report what it resolves to, and exit.
    Check,

    /// List every tool the gateway would advertise, then exit.
    Tools {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

impl Cli {
    /// Where to look for configuration when `--config` was not given.
    pub fn config_path(&self) -> PathBuf {
        if let Some(path) = &self.config {
            return path.clone();
        }
        let local = PathBuf::from("omni-mcp.toml");
        if local.exists() {
            return local;
        }
        dirs_config_home().map(|dir| dir.join("omni-mcp/omni-mcp.toml")).unwrap_or(local)
    }

    pub fn command(&self) -> &Command {
        if self.stdio {
            return &Command::Stdio;
        }
        self.command.as_ref().unwrap_or(&Command::Stdio)
    }
}

/// `$XDG_CONFIG_HOME`, else `$HOME/.config`.
fn dirs_config_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    std::env::var("HOME").ok().map(|home| PathBuf::from(home).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        <Cli as CommandFactory>::command().debug_assert();
    }

    #[test]
    fn stdio_is_the_default_command() {
        let cli = Cli::parse_from(["omni-mcp"]);
        assert!(matches!(cli.command(), Command::Stdio));
    }

    #[test]
    fn the_legacy_stdio_flag_still_selects_stdio_mode() {
        // IDE configurations in the wild pass `--stdio`; keep them working.
        assert!(matches!(Cli::parse_from(["omni-mcp", "--stdio"]).command(), Command::Stdio));
        assert!(matches!(Cli::parse_from(["omni-mcp", "stdio"]).command(), Command::Stdio));
    }

    #[test]
    fn serve_accepts_a_bind_override_and_the_unauthenticated_escape_hatch() {
        let cli = Cli::parse_from([
            "omni-mcp",
            "serve",
            "--bind",
            "0.0.0.0:9000",
            "--allow-unauthenticated",
        ]);
        let Command::Serve { bind, allow_unauthenticated } = cli.command() else {
            panic!("expected serve")
        };
        assert_eq!(bind.as_deref(), Some("0.0.0.0:9000"));
        assert!(allow_unauthenticated);
    }

    #[test]
    fn an_explicit_config_path_wins() {
        let cli = Cli::parse_from(["omni-mcp", "--config", "/etc/omni.toml"]);
        assert_eq!(cli.config_path(), PathBuf::from("/etc/omni.toml"));
    }

    #[test]
    fn the_tools_subcommand_has_a_json_flag() {
        let cli = Cli::parse_from(["omni-mcp", "tools", "--json"]);
        assert!(matches!(cli.command(), Command::Tools { json: true }));
    }
}
