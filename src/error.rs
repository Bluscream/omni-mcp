//! Crate-wide error types.
//!
//! Backend failures are deliberately *not* fatal: a dead sidecar or an
//! unreachable proxy must degrade to a tool-level error message, never take
//! down the gateway and with it the client's whole MCP session.

use crate::protocol::code;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config at {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("config at {path} is not valid TOML: {source}")]
    Parse {
        path: String,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("config references ${{{var}}} but that environment variable is not set")]
    MissingEnv { var: String },

    #[error("invalid duration {value:?}: expected a form like \"30s\", \"1500ms\" or \"2m\"")]
    Duration { value: String },

    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Something went wrong while talking to a backend, or while a tool ran.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The caller supplied bad arguments. Deterministic; retrying will not help.
    #[error("{0}")]
    InvalidArguments(String),

    /// The named tool is not registered anywhere.
    #[error("tool {0:?} is not available; call `omni_status` to list active backends")]
    NotFound(String),

    /// The tool ran but failed. The model should see this and adapt.
    #[error("{0}")]
    Failed(String),

    /// The backend did not answer within its deadline.
    #[error("{tool:?} exceeded its {seconds}s timeout")]
    Timeout { tool: String, seconds: u64 },

    /// The backend is unreachable or broken (spawn failure, connection refused).
    #[error("backend {backend:?} is unavailable: {reason}")]
    Unavailable { backend: String, reason: String },

    /// The operation is disabled by policy in the config.
    #[error("{0}")]
    Denied(String),
}

impl ToolError {
    /// JSON-RPC code used when a fault has to surface at the protocol level.
    pub fn rpc_code(&self) -> i32 {
        match self {
            Self::InvalidArguments(_) => code::INVALID_PARAMS,
            Self::NotFound(_) => code::METHOD_NOT_FOUND,
            Self::Timeout { .. } => code::TIMEOUT,
            Self::Failed(_) | Self::Unavailable { .. } | Self::Denied(_) => code::INTERNAL_ERROR,
        }
    }
}

pub type ToolResult<T> = Result<T, ToolError>;

/// Helper for the very common "required string argument" extraction.
pub fn missing(field: &str) -> ToolError {
    ToolError::InvalidArguments(format!("missing required argument {field:?}"))
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("could not bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "HTTP mode exposes every tool, including code execution, to any local process. \
         Set `server.auth_token` (e.g. auth_token = \"${{OMNI_MCP_TOKEN}}\") or pass \
         --allow-unauthenticated to acknowledge the risk."
    )]
    UnauthenticatedHttp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_errors_map_to_invalid_params() {
        assert_eq!(ToolError::InvalidArguments("x".into()).rpc_code(), code::INVALID_PARAMS);
        assert_eq!(ToolError::NotFound("x".into()).rpc_code(), code::METHOD_NOT_FOUND);
        assert_eq!(ToolError::Timeout { tool: "x".into(), seconds: 1 }.rpc_code(), code::TIMEOUT);
    }

    #[test]
    fn missing_helper_names_the_field() {
        assert!(missing("path").to_string().contains("\"path\""));
    }
}
