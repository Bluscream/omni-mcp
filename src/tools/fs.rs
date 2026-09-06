//! Filesystem search and replace.
//!
//! The previous `grep_search` corrupted every file it touched: it split on
//! lines, then rejoined with `"\n"`, which silently converted CRLF files to LF
//! and deleted the trailing newline. It also walked into `.git`, read binaries
//! as UTF-8, and wrote changes with no way to preview them first. Replacement
//! is now opt-in (`apply: true`), previewed by default, byte-exact for line
//! endings, and written atomically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::{Value, json};

use super::{NativeTool, ToolContext, args, unknown};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};

pub struct FsTools;

const DEFAULT_MAX_RESULTS: u64 = 500;
const DEFAULT_MAX_DEPTH: u64 = 16;

#[async_trait]
impl NativeTool for FsTools {
    fn descriptors(&self) -> Vec<Tool> {
        vec![Tool::new(
            "grep_search",
            "Searches files under a path for a literal string or regular expression, skipping \
             binaries and anything ignored by .gitignore. With `apply: true` and a `replace` \
             value it rewrites matches in place, preserving line endings; otherwise it only \
             previews what would change.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute file or directory path to search"
                    },
                    "pattern": { "type": "string", "description": "Text or regex to find" },
                    "is_regex": {
                        "type": "boolean",
                        "description": "Treat `pattern` as a regular expression (default false)"
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Match case exactly (default true)"
                    },
                    "replace": {
                        "type": "string",
                        "description": "Replacement text. Regex captures like $1 are expanded."
                    },
                    "apply": {
                        "type": "boolean",
                        "description": "Write the replacement to disk. Default false: preview only."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Only search files matching this glob, e.g. \"*.rs\""
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Directory recursion depth (default 16)"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum matching lines to report (default 500)"
                    }
                },
                "required": ["path", "pattern"]
            }),
        )]
    }

    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        if name != "grep_search" {
            return Err(unknown(name));
        }
        let request = SearchRequest::parse(&args, ctx)?;
        // Walking a large tree is blocking work; keep it off the reactor.
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || request.run(&ctx))
            .await
            .map_err(|e| ToolError::Failed(format!("search task failed: {e}")))?
    }
}

struct SearchRequest {
    root: PathBuf,
    matcher: Matcher,
    replace: Option<String>,
    apply: bool,
    glob: Option<String>,
    max_depth: usize,
    max_results: usize,
}

enum Matcher {
    Literal { needle: String, case_sensitive: bool },
    Regex(regex::Regex),
}

impl Matcher {
    fn find(&self, line: &str) -> bool {
        match self {
            Self::Literal { needle, case_sensitive: true } => line.contains(needle.as_str()),
            Self::Literal { needle, case_sensitive: false } => {
                line.to_lowercase().contains(&needle.to_lowercase())
            }
            Self::Regex(regex) => regex.is_match(line),
        }
    }

    fn replace(&self, line: &str, with: &str) -> String {
        match self {
            Self::Literal { needle, case_sensitive: true } => line.replace(needle.as_str(), with),
            Self::Literal { needle, case_sensitive: false } => {
                replace_ignore_case(line, needle, with)
            }
            Self::Regex(regex) => regex.replace_all(line, with).into_owned(),
        }
    }
}

fn replace_ignore_case(haystack: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let lower_haystack = haystack.to_lowercase();
    let lower_needle = needle.to_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;

    while let Some(offset) = lower_haystack[cursor..].find(&lower_needle) {
        let start = cursor + offset;
        let end = start + lower_needle.len();
        // Case folding can change byte length; fall back to a literal copy when
        // the offsets do not land on character boundaries.
        if !haystack.is_char_boundary(start) || !haystack.is_char_boundary(end) {
            break;
        }
        out.push_str(&haystack[cursor..start]);
        out.push_str(with);
        cursor = end;
    }
    out.push_str(&haystack[cursor..]);
    out
}

impl SearchRequest {
    fn parse(arguments: &Value, ctx: &ToolContext) -> ToolResult<Self> {
        let root = ctx.resolve(args::string(arguments, "path")?)?;
        if !root.exists() {
            return Err(ToolError::InvalidArguments(format!(
                "path {} does not exist",
                root.display()
            )));
        }

        let pattern = args::string(arguments, "pattern")?;
        if pattern.is_empty() {
            return Err(ToolError::InvalidArguments("pattern must not be empty".into()));
        }
        let case_sensitive = args::bool_or(arguments, "case_sensitive", true)?;
        let matcher = if args::bool_or(arguments, "is_regex", false)? {
            let mut builder = regex::RegexBuilder::new(pattern);
            builder.case_insensitive(!case_sensitive).size_limit(1 << 20);
            Matcher::Regex(builder.build().map_err(|e| {
                ToolError::InvalidArguments(format!("invalid regular expression: {e}"))
            })?)
        } else {
            Matcher::Literal { needle: pattern.to_string(), case_sensitive }
        };

        let replace = args::opt_string(arguments, "replace")?.map(str::to_string);
        let apply = args::bool_or(arguments, "apply", false)?;
        if apply {
            if replace.is_none() {
                return Err(ToolError::InvalidArguments(
                    "`apply` was set but no `replace` value was given".into(),
                ));
            }
            ctx.require_file_mutation()?;
        }

        Ok(Self {
            root,
            matcher,
            replace,
            apply,
            glob: args::opt_string(arguments, "glob")?.map(str::to_string),
            max_depth: usize::try_from(args::u64_or(arguments, "max_depth", DEFAULT_MAX_DEPTH)?)
                .unwrap_or(usize::MAX),
            max_results: usize::try_from(args::u64_or(
                arguments,
                "max_results",
                DEFAULT_MAX_RESULTS,
            )?)
            .unwrap_or(usize::MAX),
        })
    }

    fn run(&self, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        let mut hits: Vec<Value> = Vec::new();
        let mut total_matches = 0u64;
        let mut files_changed: BTreeMap<String, u64> = BTreeMap::new();
        let mut skipped = 0u64;

        for path in self.candidate_files(ctx) {
            let Ok(content) = std::fs::read(&path) else {
                skipped += 1;
                continue;
            };
            // A NUL byte in the first 8 KiB is the standard binary heuristic.
            if memchr::memchr(0, &content[..content.len().min(8192)]).is_some() {
                skipped += 1;
                continue;
            }
            let Ok(content) = String::from_utf8(content) else {
                skipped += 1;
                continue;
            };

            if let Some(rewritten) = self.scan_file(&path, &content, &mut hits, &mut total_matches)
            {
                let changed = files_changed.entry(path.display().to_string()).or_insert(0);
                *changed += 1;
                if self.apply {
                    write_atomically(&path, &rewritten)?;
                }
            }
        }

        Ok(CallToolResult::structured(json!({
            "root": self.root.display().to_string(),
            "matches": total_matches,
            "truncated": total_matches > hits.len() as u64,
            "results": hits,
            "files_modified": if self.apply { files_changed.keys().collect() } else { Vec::new() },
            "files_would_be_modified": if self.apply { Vec::new() } else { files_changed.keys().collect() },
            "applied": self.apply,
            "unreadable_or_binary_files_skipped": skipped
        })))
    }

    fn candidate_files(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        if self.root.is_file() {
            return vec![self.root.clone()];
        }

        let mut builder = WalkBuilder::new(&self.root);
        builder
            .max_depth(Some(self.max_depth))
            .standard_filters(true) // honours .gitignore, skips hidden files and .git
            .follow_links(false)
            .max_filesize(Some(ctx.max_file_bytes()));

        if let Some(glob) = &self.glob {
            let mut globs = ignore::overrides::OverrideBuilder::new(&self.root);
            if globs.add(glob).is_ok() {
                if let Ok(built) = globs.build() {
                    builder.overrides(built);
                }
            }
        }

        builder
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
            .map(ignore::DirEntry::into_path)
            .collect()
    }

    /// Returns the rewritten content when a replacement changed something.
    fn scan_file(
        &self,
        path: &Path,
        content: &str,
        hits: &mut Vec<Value>,
        total: &mut u64,
    ) -> Option<String> {
        // `split_inclusive` keeps each line's own terminator, so CRLF files stay
        // CRLF and a file with no trailing newline keeps not having one.
        let mut rebuilt = String::with_capacity(content.len());
        let mut modified = false;

        for (index, raw_line) in content.split_inclusive('\n').enumerate() {
            let terminator_len = raw_line.len() - raw_line.trim_end_matches(['\n', '\r']).len();
            let (body, terminator) = raw_line.split_at(raw_line.len() - terminator_len);

            if !self.matcher.find(body) {
                rebuilt.push_str(raw_line);
                continue;
            }
            *total += 1;

            let replaced = self.replace.as_ref().map(|with| self.matcher.replace(body, with));
            match &replaced {
                Some(new_body) if new_body != body => {
                    modified = true;
                    rebuilt.push_str(new_body);
                    rebuilt.push_str(terminator);
                }
                _ => rebuilt.push_str(raw_line),
            }

            if hits.len() < self.max_results {
                let mut hit = json!({
                    "file": path.display().to_string(),
                    "line": index + 1,
                    "text": body
                });
                if let Some(new_body) = replaced.filter(|n| n != body) {
                    hit["replacement"] = json!(new_body);
                }
                hits.push(hit);
            }
        }

        modified.then_some(rebuilt)
    }
}

/// Writes via a temporary file in the same directory then renames, so a crash
/// mid-write cannot leave a half-rewritten source file behind.
fn write_atomically(path: &Path, content: &str) -> ToolResult<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let failed = |e: std::io::Error| ToolError::Failed(format!("writing {}: {e}", path.display()));

    let mut temp = tempfile::NamedTempFile::new_in(directory).map_err(failed)?;
    std::io::Write::write_all(&mut temp, content.as_bytes()).map_err(failed)?;

    // Preserve the original mode; `NamedTempFile` creates files as 0600.
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = temp
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(metadata.permissions().mode()));
    }

    temp.persist(path).map_err(|e| {
        ToolError::Failed(format!("could not replace {}: {}", path.display(), e.error))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests;
