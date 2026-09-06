//! Pure text tools: diffing, regex matching and counting. No I/O, no policy.

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use super::{NativeTool, ToolContext, args, unknown};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};

pub struct TextTools;

/// Guards against a pathological pattern turning a tool call into a hang.
const MAX_REGEX_SIZE: usize = 1 << 20;
/// Caps how many matches are returned so a broad pattern cannot blow up the
/// model's context window.
const DEFAULT_MATCH_LIMIT: u64 = 1_000;

#[async_trait]
impl NativeTool for TextTools {
    fn descriptors(&self) -> Vec<Tool> {
        vec![
            Tool::new(
                "diff_text",
                "Computes a unified diff between two strings. Returns the changed hunks with \
                 line numbers, not the whole file.",
                json!({
                    "type": "object",
                    "properties": {
                        "old_text": { "type": "string", "description": "Original text" },
                        "new_text": { "type": "string", "description": "Modified text" },
                        "context_lines": {
                            "type": "integer",
                            "description": "Unchanged lines to keep around each hunk (default 3)"
                        }
                    },
                    "required": ["old_text", "new_text"]
                }),
            ),
            Tool::new(
                "diff_json",
                "Compares two JSON values structurally and returns a unified diff of their \
                 canonical pretty-printed forms. Accepts objects or JSON strings.",
                json!({
                    "type": "object",
                    "properties": {
                        "old_json": { "description": "Original JSON value or a JSON string" },
                        "new_json": { "description": "Modified JSON value or a JSON string" },
                        "context_lines": {
                            "type": "integer",
                            "description": "Unchanged lines to keep around each hunk (default 3)"
                        }
                    },
                    "required": ["old_json", "new_json"]
                }),
            ),
            Tool::new(
                "regex_match",
                "Finds all matches of a Rust-syntax regular expression in a string, returning \
                 each match with its byte offset and capture groups.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regular expression" },
                        "text": { "type": "string", "description": "Text to search" },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum matches to return (default 1000)"
                        }
                    },
                    "required": ["pattern", "text"]
                }),
            ),
            Tool::new(
                "count_stats",
                "Counts lines, words, Unicode characters and bytes in a string.",
                json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Text to measure" }
                    },
                    "required": ["text"]
                }),
            ),
        ]
    }

    async fn call(
        &self,
        name: &str,
        args: Value,
        _ctx: &ToolContext,
    ) -> ToolResult<CallToolResult> {
        match name {
            "diff_text" => diff_text(&args),
            "diff_json" => diff_json(&args),
            "regex_match" => regex_match(&args),
            "count_stats" => Ok(count_stats(&args)?),
            other => Err(unknown(other)),
        }
    }
}

fn diff_text(arguments: &Value) -> ToolResult<CallToolResult> {
    let old = args::string(arguments, "old_text")?;
    let new = args::string(arguments, "new_text")?;
    let context = usize::try_from(args::u64_or(arguments, "context_lines", 3)?).unwrap_or(3);
    Ok(render_diff(old, new, context))
}

fn diff_json(arguments: &Value) -> ToolResult<CallToolResult> {
    let old = canonical_json(arguments, "old_json")?;
    let new = canonical_json(arguments, "new_json")?;
    let context = usize::try_from(args::u64_or(arguments, "context_lines", 3)?).unwrap_or(3);
    Ok(render_diff(&old, &new, context))
}

/// Accepts either an inline JSON value or a JSON-encoded string, so callers
/// that pass file contents verbatim get a structural diff rather than a
/// character-by-character one.
fn canonical_json(arguments: &Value, field: &str) -> ToolResult<String> {
    let value = arguments.get(field).ok_or_else(|| crate::error::missing(field))?;
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).map_err(|e| {
            ToolError::InvalidArguments(format!("{field:?} is a string but not valid JSON: {e}"))
        })?,
        other => other.clone(),
    };
    serde_json::to_string_pretty(&parsed)
        .map_err(|e| ToolError::Failed(format!("could not render {field:?}: {e}")))
}

/// Produces a unified diff. The previous implementation emitted every line of
/// both inputs including unchanged ones, which for a large file meant the
/// entire file came back twice.
fn render_diff(old: &str, new: &str, context: usize) -> CallToolResult {
    use std::fmt::Write as _;

    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();
    let mut hunks = 0usize;

    for group in diff.grouped_ops(context.min(32)) {
        hunks += 1;
        if let (Some(first), Some(last)) = (group.first(), group.last()) {
            let old_range = first.old_range().start..last.old_range().end;
            let new_range = first.new_range().start..last.new_range().end;
            let _ = writeln!(
                output,
                "@@ -{},{} +{},{} @@",
                old_range.start + 1,
                old_range.len(),
                new_range.start + 1,
                new_range.len()
            );
        }
        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                output.push(sign);
                output.push_str(change.value());
                if !change.value().ends_with('\n') {
                    output.push('\n');
                }
            }
        }
    }

    if hunks == 0 {
        return CallToolResult::text("(inputs are identical)");
    }
    CallToolResult::text(output)
}

fn regex_match(arguments: &Value) -> ToolResult<CallToolResult> {
    let pattern = args::string(arguments, "pattern")?;
    let text = args::string(arguments, "text")?;
    let limit = args::u64_or(arguments, "limit", DEFAULT_MATCH_LIMIT)?;

    let regex = compile(pattern)?;
    let mut matches = Vec::new();
    let mut total = 0u64;

    for capture in regex.captures_iter(text) {
        total += 1;
        if matches.len() as u64 >= limit {
            continue;
        }
        let whole = capture.get(0).map_or("", |m| m.as_str());
        let start = capture.get(0).map_or(0, |m| m.start());
        let groups: Vec<Value> = capture
            .iter()
            .skip(1)
            .map(|group| group.map_or(Value::Null, |g| json!(g.as_str())))
            .collect();
        matches.push(json!({ "text": whole, "offset": start, "groups": groups }));
    }

    Ok(CallToolResult::structured(json!({
        "count": total,
        "truncated": total > matches.len() as u64,
        "matches": matches
    })))
}

/// Compiles a pattern with a size bound, converting the failure into an
/// argument error rather than propagating an opaque regex panic path.
pub fn compile(pattern: &str) -> ToolResult<Regex> {
    regex::RegexBuilder::new(pattern)
        .size_limit(MAX_REGEX_SIZE)
        .build()
        .map_err(|e| ToolError::InvalidArguments(format!("invalid regular expression: {e}")))
}

fn count_stats(arguments: &Value) -> ToolResult<CallToolResult> {
    let text = args::string(arguments, "text")?;
    Ok(CallToolResult::structured(json!({
        "lines": text.lines().count(),
        "words": text.split_whitespace().count(),
        "chars": text.chars().count(),
        "bytes": text.len()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolPolicy;

    fn ctx() -> ToolContext {
        ToolContext::new(ToolPolicy::default())
    }

    async fn call(name: &str, arguments: Value) -> ToolResult<String> {
        let result = TextTools.call(name, arguments, &ctx()).await?;
        let crate::protocol::Content::Text { text } = &result.content[0] else {
            panic!("expected a text block")
        };
        Ok(text.clone())
    }

    #[tokio::test]
    async fn diff_reports_only_changed_hunks() {
        use std::fmt::Write as _;

        let mut old = String::new();
        for i in 1..=100 {
            let _ = writeln!(old, "line {i}");
        }
        let new = old.replace("line 50\n", "CHANGED\n");

        let text = call("diff_text", json!({ "old_text": old, "new_text": new })).await.unwrap();
        assert!(text.contains("-line 50"));
        assert!(text.contains("+CHANGED"));
        // The old implementation echoed all 100 unchanged lines twice.
        assert!(!text.contains("line 10\n"), "unrelated context leaked into the diff");
        assert!(text.starts_with("@@"));
    }

    #[tokio::test]
    async fn identical_inputs_say_so_instead_of_returning_the_whole_file() {
        let text =
            call("diff_text", json!({ "old_text": "a\nb\n", "new_text": "a\nb\n" })).await.unwrap();
        assert_eq!(text, "(inputs are identical)");
    }

    #[tokio::test]
    async fn json_diff_ignores_key_order_and_formatting() {
        let text = call(
            "diff_json",
            json!({ "old_json": { "a": 1, "b": 2 }, "new_json": { "a": 1, "b": 2 } }),
        )
        .await
        .unwrap();
        assert_eq!(text, "(inputs are identical)");
    }

    #[tokio::test]
    async fn json_diff_accepts_encoded_strings() {
        let text = call("diff_json", json!({ "old_json": "{\"a\":1}", "new_json": "{\"a\":2}" }))
            .await
            .unwrap();
        assert!(text.contains("-  \"a\": 1"));
        assert!(text.contains("+  \"a\": 2"));
    }

    #[tokio::test]
    async fn json_diff_rejects_a_string_that_is_not_json() {
        let err = call("diff_json", json!({ "old_json": "not json", "new_json": "{}" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn regex_match_returns_offsets_and_capture_groups() {
        let text = call("regex_match", json!({ "pattern": r"(\w+)@(\w+)", "text": "a@b and c@d" }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["count"], 2);
        assert_eq!(parsed["matches"][0]["groups"], json!(["a", "b"]));
        assert_eq!(parsed["matches"][1]["offset"], json!(8));
    }

    #[tokio::test]
    async fn regex_match_truncates_and_reports_that_it_did() {
        let haystack = "x".repeat(50);
        let text = call("regex_match", json!({ "pattern": "x", "text": haystack, "limit": 5 }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["count"], 50);
        assert_eq!(parsed["truncated"], json!(true));
        assert_eq!(parsed["matches"].as_array().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn an_invalid_pattern_is_an_argument_error() {
        let err =
            call("regex_match", json!({ "pattern": "(unclosed", "text": "x" })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn missing_arguments_are_reported_rather_than_defaulted_to_empty() {
        // The old code searched for "" when `pattern` was omitted.
        let err = call("regex_match", json!({ "text": "x" })).await.unwrap_err();
        assert!(err.to_string().contains("\"pattern\""));
    }

    #[tokio::test]
    async fn count_stats_counts_unicode_characters_not_bytes() {
        let text =
            call("count_stats", json!({ "text": "héllo wörld\nsecond line" })).await.unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["lines"], 2);
        assert_eq!(parsed["words"], 4);
        assert_eq!(parsed["chars"], 23);
        assert_eq!(parsed["bytes"], 25);
    }

    #[tokio::test]
    async fn an_unrouted_name_is_a_not_found_error() {
        assert!(matches!(call("nope", json!({})).await, Err(ToolError::NotFound(_))));
    }
}
