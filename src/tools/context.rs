//! Execution context shared by the native tools: policy checks and path
//! sandboxing.
//!
//! Previously any caller could read, rewrite or byte-patch any file the daemon
//! user could reach, and `eval_code` would run arbitrary shell — over an HTTP
//! endpoint with permissive CORS. Those capabilities are now gated on explicit
//! configuration and confined to `tools.allowed_roots`.

use std::path::{Component, Path, PathBuf};

use crate::config::ToolPolicy;
use crate::error::{ToolError, ToolResult};

#[derive(Debug, Clone)]
pub struct ToolContext {
    policy: ToolPolicy,
    canonical_roots: Vec<PathBuf>,
}

impl ToolContext {
    pub fn new(policy: ToolPolicy) -> Self {
        // Roots are canonicalised once so that symlinked roots (`/tmp` on
        // macOS, `/home` on many Linux setups) still match.
        let canonical_roots = policy
            .allowed_roots
            .iter()
            .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
            .collect();
        Self { policy, canonical_roots }
    }

    pub fn policy(&self) -> &ToolPolicy {
        &self.policy
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.policy.max_file_bytes
    }

    /// Fails unless `tools.allow_code_execution` is set.
    pub fn require_code_execution(&self) -> ToolResult<()> {
        if self.policy.allow_code_execution {
            return Ok(());
        }
        Err(ToolError::Denied(
            "code execution is disabled; set `allow_code_execution = true` under [tools] in \
             omni-mcp.toml to enable it"
                .into(),
        ))
    }

    /// Fails unless `tools.allow_ssh` is set.
    pub fn require_ssh(&self) -> ToolResult<()> {
        if self.policy.allow_ssh {
            return Ok(());
        }
        Err(ToolError::Denied(
            "ssh access is disabled; set `allow_ssh = true` under [tools] in omni-mcp.toml to enable it"
                .into(),
        ))
    }

    /// Fails unless `tools.allow_file_mutation` is set.
    pub fn require_file_mutation(&self) -> ToolResult<()> {
        if self.policy.allow_file_mutation {
            return Ok(());
        }
        Err(ToolError::Denied(
            "this tool modifies files, which is disabled; set `allow_file_mutation = true` under \
             [tools] in omni-mcp.toml to enable it"
                .into(),
        ))
    }

    /// Resolves a caller-supplied path and rejects anything outside
    /// `allowed_roots`. With no roots configured, any path is permitted.
    pub fn resolve(&self, raw: &str) -> ToolResult<PathBuf> {
        if raw.trim().is_empty() {
            return Err(ToolError::InvalidArguments("path must not be empty".into()));
        }
        let requested = Path::new(raw);
        if requested.is_relative() {
            return Err(ToolError::InvalidArguments(format!(
                "path {raw:?} must be absolute; the gateway has no meaningful working directory"
            )));
        }

        // Canonicalise when the path exists so symlinks cannot escape a root;
        // otherwise resolve lexically so that creating a new file still works.
        let resolved = requested.canonicalize().unwrap_or_else(|_| lexical_normalize(requested));

        if self.canonical_roots.is_empty() {
            return Ok(resolved);
        }
        if self.canonical_roots.iter().any(|root| resolved.starts_with(root)) {
            return Ok(resolved);
        }
        Err(ToolError::Denied(format!(
            "path {raw:?} is outside the configured tools.allowed_roots"
        )))
    }
}

/// Collapses `.` and `..` without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(policy: ToolPolicy) -> ToolContext {
        ToolContext::new(policy)
    }

    fn permissive() -> ToolPolicy {
        ToolPolicy { allow_code_execution: true, allow_file_mutation: true, ..Default::default() }
    }

    #[test]
    fn dangerous_capabilities_are_denied_by_default() {
        let ctx = context(ToolPolicy::default());
        assert!(matches!(ctx.require_code_execution(), Err(ToolError::Denied(_))));
        assert!(matches!(ctx.require_file_mutation(), Err(ToolError::Denied(_))));
    }

    #[test]
    fn denial_messages_say_how_to_enable_the_capability() {
        let err = context(ToolPolicy::default()).require_code_execution().unwrap_err();
        assert!(err.to_string().contains("allow_code_execution = true"));
    }

    #[test]
    fn capabilities_are_granted_when_configured() {
        let ctx = context(permissive());
        assert!(ctx.require_code_execution().is_ok());
        assert!(ctx.require_file_mutation().is_ok());
    }

    #[test]
    fn without_roots_any_absolute_path_resolves() {
        let ctx = context(ToolPolicy::default());
        assert!(ctx.resolve("/etc/hostname").is_ok());
    }

    #[test]
    fn relative_and_empty_paths_are_rejected() {
        let ctx = context(ToolPolicy::default());
        assert!(ctx.resolve("relative/file").is_err());
        assert!(ctx.resolve("   ").is_err());
    }

    #[test]
    fn paths_inside_a_root_are_allowed_and_outside_are_denied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("inside.txt"), b"x").unwrap();

        let ctx = context(ToolPolicy { allowed_roots: vec![root.clone()], ..permissive() });
        assert!(ctx.resolve(root.join("inside.txt").to_str().unwrap()).is_ok());
        assert!(matches!(ctx.resolve("/etc/passwd"), Err(ToolError::Denied(_))));
    }

    #[test]
    fn dot_dot_cannot_escape_an_allowed_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = context(ToolPolicy { allowed_roots: vec![root.clone()], ..permissive() });

        let escape = format!("{}/../../../../etc/passwd", root.display());
        assert!(matches!(ctx.resolve(&escape), Err(ToolError::Denied(_))));
    }

    #[test]
    fn a_symlink_pointing_outside_a_root_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let link = root.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();

        let ctx = context(ToolPolicy { allowed_roots: vec![root], ..permissive() });
        #[cfg(unix)]
        assert!(matches!(ctx.resolve(link.to_str().unwrap()), Err(ToolError::Denied(_))));
    }

    #[test]
    fn a_not_yet_existing_file_inside_a_root_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = context(ToolPolicy { allowed_roots: vec![root.clone()], ..permissive() });
        assert!(ctx.resolve(root.join("new-file.resx").to_str().unwrap()).is_ok());
    }

    #[test]
    fn lexical_normalisation_collapses_dot_segments() {
        assert_eq!(lexical_normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
    }
}
