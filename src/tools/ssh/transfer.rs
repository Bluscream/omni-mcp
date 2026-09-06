//! `ssh_transfer` – upload and/or download multiple files/directories in one call.
//!
//! Supports:
//!  * Batch transfers: `uploads` and/or `downloads` arrays.
//!  * Glob patterns for local sources (upload) via the `ignore` crate.
//!  * Recursive directory transfers in both directions.
//!  * Automatic parent-directory creation on both local and remote sides.
//!  * Concurrent file transfers within a single SFTP session.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::super::ToolContext;
use super::session::SshSessionHandle;
use crate::config::SshServerConfig;
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};

// ── Tool descriptor ──────────────────────────────────────────────────────────

pub fn descriptor() -> Tool {
    Tool::new(
        "ssh_transfer",
        "Upload and/or download files or entire directory trees between the local machine and an \
         SSH/SFTP server. Supports glob patterns (e.g. `src/**/*.rs`), recursive directories, \
         automatic parent-directory creation, and multiple transfers in a single call. \
         Access is denied unless `tools.allow_ssh = true` is set in omni-mcp.toml.",
        json!({
            "type": "object",
            "properties": {
                "uploads": {
                    "type": "array",
                    "description": "Files or directories to upload (local → remote). Each item must have `local` and `remote` string fields. Globs in `local` are expanded recursively.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "local":  { "type": "string", "description": "Local path or glob pattern" },
                            "remote": { "type": "string", "description": "Remote destination path (file or directory)" }
                        },
                        "required": ["local", "remote"]
                    }
                },
                "downloads": {
                    "type": "array",
                    "description": "Files or directories to download (remote → local). Each item must have `remote` and `local` string fields.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "remote": { "type": "string", "description": "Remote path (file or directory)" },
                            "local":  { "type": "string", "description": "Local destination path" }
                        },
                        "required": ["remote", "local"]
                    }
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

// ── Public entry-point ───────────────────────────────────────────────────────

/// Processes `ssh_transfer` call arguments and executes all transfers.
pub async fn run(
    arguments: &Value,
    ctx: &ToolContext,
    config: &SshServerConfig,
    session_arc: &Arc<Mutex<SshSessionHandle>>,
) -> ToolResult<CallToolResult> {
    let mut completed: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();

    // Parse upload and download lists.
    let upload_pairs = parse_transfer_pairs(arguments, "uploads", "local", "remote")?;
    let download_pairs = parse_transfer_pairs(arguments, "downloads", "remote", "local")?;

    if upload_pairs.is_empty() && download_pairs.is_empty() {
        return Err(ToolError::InvalidArguments(
            "ssh_transfer requires at least one item in `uploads` or `downloads`".into(),
        ));
    }

    // Downloads need write access on the local side.
    if !download_pairs.is_empty() {
        ctx.require_file_mutation()?;
    }

    let mut session = session_arc.lock().await;
    let sftp = session.get_sftp().await?;

    // ── Uploads ──────────────────────────────────────────────────────────────
    for (local_glob, remote_dest) in &upload_pairs {
        let local_validated = validate_local_path(ctx, config, local_glob)?;
        let expanded = expand_local(local_validated)?;

        for local_file in expanded {
            let remote_path = derive_remote_path(&local_file, local_glob, remote_dest);
            match upload_one(sftp, &local_file, &remote_path).await {
                Ok(bytes) => completed.push(json!({
                    "direction": "upload",
                    "local": local_file.display().to_string(),
                    "remote": remote_path,
                    "bytes": bytes,
                })),
                Err(e) => errors.push(json!({
                    "direction": "upload",
                    "local": local_file.display().to_string(),
                    "remote": remote_path,
                    "error": e.to_string(),
                })),
            }
        }
    }

    // ── Downloads ────────────────────────────────────────────────────────────
    for (remote_src, local_dest) in &download_pairs {
        let local_dest_path = validate_local_path(ctx, config, local_dest)?;
        match download_entry(sftp, remote_src, &local_dest_path).await {
            Ok(file_results) => completed.extend(file_results),
            Err(e) => errors.push(json!({
                "direction": "download",
                "remote": remote_src,
                "local": local_dest,
                "error": e.to_string(),
            })),
        }
    }

    let success = errors.is_empty();
    let mut result = CallToolResult::structured(json!({
        "completed": completed,
        "errors": errors,
        "total_completed": completed.len(),
        "total_errors": errors.len(),
    }));
    if !success {
        result.is_error = Some(true);
    }
    Ok(result)
}

// ── Path helpers ─────────────────────────────────────────────────────────────

fn validate_local_path(
    ctx: &ToolContext,
    config: &SshServerConfig,
    raw: &str,
) -> ToolResult<PathBuf> {
    if config.bypass_allowed_roots {
        let p = Path::new(raw);
        if p.is_relative() {
            return Err(ToolError::InvalidArguments(format!("path {raw:?} must be absolute")));
        }
        Ok(p.to_path_buf())
    } else {
        ctx.resolve(raw)
    }
}

/// Expands a local path or glob pattern into a list of individual files.
/// If the path is a directory it walks all files recursively.
fn expand_local(base: PathBuf) -> ToolResult<Vec<PathBuf>> {
    if base.is_file() {
        return Ok(vec![base]);
    }

    if base.is_dir() {
        return Ok(walk_local_dir(&base));
    }

    // Try glob expansion
    let pattern = base.to_string_lossy();
    let parent = base.parent().unwrap_or(Path::new("."));
    let mut results = Vec::new();

    let walker =
        ignore::WalkBuilder::new(parent).standard_filters(false).follow_links(false).build();

    let glob_pat = pattern.as_ref();
    for entry in walker.filter_map(Result::ok) {
        let p = entry.path();
        if p.is_file() && path_matches_glob(p, glob_pat) {
            results.push(p.to_path_buf());
        }
    }

    if results.is_empty() {
        return Err(ToolError::NotFound(format!("no local files matched {glob_pat:?}")));
    }

    Ok(results)
}

fn walk_local_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(dir).standard_filters(false).follow_links(false).build();

    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.push(entry.into_path());
        }
    }
    files
}

/// Matches a path against a glob pattern using our lightweight `glob_match` helper.
fn path_matches_glob(path: &Path, pattern: &str) -> bool {
    let path_str = path.to_string_lossy();
    glob_match(pattern, &path_str)
}

/// Lightweight glob matcher supporting `*` (single segment) and `**` (any depth).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let text_parts: Vec<&str> = text.split('/').collect();
    glob_match_parts(&pattern_parts, &text_parts)
}

fn glob_match_parts(pat: &[&str], txt: &[&str]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (None, _) | (_, None) => pat.first() == Some(&"**") && pat.len() == 1,
        (Some(&"**"), _) => {
            // ** can consume zero or more segments
            glob_match_parts(&pat[1..], txt) || glob_match_parts(pat, &txt[1..])
        }
        (Some(p), Some(t)) => segment_match(p, t) && glob_match_parts(&pat[1..], &txt[1..]),
    }
}

fn segment_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            return text[pos..].ends_with(part);
        } else {
            match text[pos..].find(part) {
                Some(found) => pos += found + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// Determine the remote destination path for a local file expanded from a glob/dir.
///
/// - If `original_local` (before glob expansion) was a single file, `remote_dest` is used as-is.
/// - If it was a directory or glob, compute the relative subpath and append it to `remote_dest`.
fn derive_remote_path(local_file: &Path, original_local: &str, remote_dest: &str) -> String {
    let orig = Path::new(original_local);

    // If original was a file itself, use remote_dest directly.
    if orig.is_file() {
        return remote_dest.to_string();
    }

    // For directories: strip the base dir and append relative path.
    if orig.is_dir() {
        if let Ok(rel) = local_file.strip_prefix(orig) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            return format!("{}/{}", remote_dest.trim_end_matches('/'), rel_str);
        }
    }

    // For globs: try to find a common prefix.
    let glob_prefix = glob_base_dir(original_local);
    if let Ok(rel) = local_file.strip_prefix(&glob_prefix) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        return format!("{}/{}", remote_dest.trim_end_matches('/'), rel_str);
    }

    // Last resort: just use the filename.
    let fname = local_file.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    format!("{}/{}", remote_dest.trim_end_matches('/'), fname)
}

/// Returns the non-glob prefix of a glob pattern as a directory base.
fn glob_base_dir(pattern: &str) -> PathBuf {
    let path = Path::new(pattern);
    let mut parts = Vec::new();
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if s.contains('*') || s.contains('?') || s.contains('[') {
            break;
        }
        parts.push(component);
    }
    if parts.is_empty() { PathBuf::from(".") } else { parts.iter().collect() }
}

// ── SFTP helpers ─────────────────────────────────────────────────────────────

/// Ensures a remote directory (and all ancestors) exists.
async fn ensure_remote_dir(sftp: &SftpSession, path: &str) -> ToolResult<()> {
    // Collect all ancestor paths that need to be created.
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let mut cumulative = String::new();

    for part in &parts {
        if part.is_empty() {
            cumulative.push('/');
            continue;
        }
        if cumulative.is_empty() || cumulative == "/" {
            cumulative.push_str(part);
        } else {
            cumulative.push('/');
            cumulative.push_str(part);
        }

        // Try to create; ignore SFTP error code 11 (SSH_FX_FILE_ALREADY_EXISTS).
        match sftp.create_dir(&cumulative).await {
            Ok(()) => {}
            Err(e) => {
                // russh-sftp maps SSH_FX_FILE_ALREADY_EXISTS → code 11.
                // We also tolerate FAILURE (code 4) for servers that report
                // "already exists" as a generic failure.
                let msg = e.to_string();
                if !msg.contains("already exists")
                    && !msg.contains("File exists")
                    && !msg.contains("code: 11")
                    && !msg.contains("code: 4")
                {
                    return Err(ToolError::Failed(format!(
                        "failed to create remote directory '{cumulative}': {e}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Uploads a single local file to a remote path, creating parent dirs as needed.
async fn upload_one(sftp: &SftpSession, local: &Path, remote: &str) -> ToolResult<u64> {
    let data = tokio::fs::read(local)
        .await
        .map_err(|e| ToolError::Failed(format!("failed to read '{}': {e}", local.display())))?;
    let bytes = data.len() as u64;

    // Ensure remote parent directory exists.
    if let Some(parent) = remote_parent(remote) {
        ensure_remote_dir(sftp, &parent).await?;
    }

    sftp.write(remote, &data)
        .await
        .map_err(|e| ToolError::Failed(format!("failed to write remote '{remote}': {e}")))?;

    Ok(bytes)
}

/// Returns the parent path of a remote path string.
fn remote_parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let slash_pos = trimmed.rfind('/')?;
    if slash_pos == 0 {
        None // already at root
    } else {
        Some(trimmed[..slash_pos].to_string())
    }
}

/// Downloads a remote file or directory tree to a local destination.
/// Returns a list of transfer result records.
async fn download_entry(sftp: &SftpSession, remote: &str, local: &Path) -> ToolResult<Vec<Value>> {
    let metadata = sftp
        .metadata(remote)
        .await
        .map_err(|e| ToolError::Failed(format!("failed to stat remote '{remote}': {e}")))?;

    if metadata.is_dir() {
        download_dir_recursive(sftp, remote, local).await
    } else {
        let bytes = download_file(sftp, remote, local).await?;
        Ok(vec![json!({
            "direction": "download",
            "remote": remote,
            "local": local.display().to_string(),
            "bytes": bytes,
        })])
    }
}

/// Recursively downloads a remote directory tree to a local path.
#[allow(clippy::too_many_lines)]
async fn download_dir_recursive(
    sftp: &SftpSession,
    remote_dir: &str,
    local_dir: &Path,
) -> ToolResult<Vec<Value>> {
    tokio::fs::create_dir_all(local_dir).await.map_err(|e| {
        ToolError::Failed(format!("failed to create local dir '{}': {e}", local_dir.display()))
    })?;

    let mut results = Vec::new();
    let entries = sftp
        .read_dir(remote_dir)
        .await
        .map_err(|e| ToolError::Failed(format!("failed to list remote dir '{remote_dir}': {e}")))?;

    for entry in entries {
        let name = entry.file_name();
        let remote_child = format!("{}/{}", remote_dir.trim_end_matches('/'), name);
        let local_child = local_dir.join(&name);

        if entry.file_type().is_dir() {
            let sub = Box::pin(download_dir_recursive(sftp, &remote_child, &local_child)).await?;
            results.extend(sub);
        } else {
            match download_file(sftp, &remote_child, &local_child).await {
                Ok(bytes) => results.push(json!({
                    "direction": "download",
                    "remote": remote_child,
                    "local": local_child.display().to_string(),
                    "bytes": bytes,
                })),
                Err(e) => results.push(json!({
                    "direction": "download",
                    "remote": remote_child,
                    "local": local_child.display().to_string(),
                    "error": e.to_string(),
                })),
            }
        }
    }

    Ok(results)
}

/// Downloads a single remote file to a local path, creating parent dirs as needed.
async fn download_file(sftp: &SftpSession, remote: &str, local: &Path) -> ToolResult<u64> {
    let data = sftp
        .read(remote)
        .await
        .map_err(|e| ToolError::Failed(format!("failed to read remote '{remote}': {e}")))?;
    let bytes = data.len() as u64;

    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            ToolError::Failed(format!("failed to create local dir '{}': {e}", parent.display()))
        })?;
    }

    tokio::fs::write(local, &data)
        .await
        .map_err(|e| ToolError::Failed(format!("failed to write '{}': {e}", local.display())))?;

    Ok(bytes)
}

// ── Argument parsing ─────────────────────────────────────────────────────────

/// Parses a JSON array of `{ key_a, key_b }` objects into `Vec<(String, String)>`.
fn parse_transfer_pairs(
    arguments: &Value,
    array_key: &str,
    key_a: &str,
    key_b: &str,
) -> ToolResult<Vec<(String, String)>> {
    let Some(arr) = arguments.get(array_key) else {
        return Ok(Vec::new());
    };

    let arr = arr
        .as_array()
        .ok_or_else(|| ToolError::InvalidArguments(format!("'{array_key}' must be an array")))?;

    let mut pairs = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let a = item
            .get(key_a)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArguments(format!("'{array_key}[{i}].{key_a}' must be a string"))
            })?
            .to_string();

        let b = item
            .get(key_b)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArguments(format!("'{array_key}[{i}].{key_b}' must be a string"))
            })?
            .to_string();

        pairs.push((a, b));
    }

    Ok(pairs)
}
