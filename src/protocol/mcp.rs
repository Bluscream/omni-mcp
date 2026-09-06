//! MCP payload types (`Tool`, `CallToolResult`, content blocks).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// A tool advertised over `tools/list`.
///
/// Unknown fields from upstream servers are preserved in `extra` so that
/// proxying a tool through omni-mcp is lossless — annotations, `title`,
/// `outputSchema` and future spec additions survive the round trip.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default = "empty_object_schema")]
    pub input_schema: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            input_schema,
            extra: Map::new(),
        }
    }

    /// Rewrites the advertised name, e.g. to apply a backend namespace prefix.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

/// Content block of a tool result. Only the variants omni-mcp produces or
/// forwards are modelled explicitly; anything else round-trips as `Other`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text {
        text: String,
    },
    #[serde(untagged)]
    Other(Value),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// Result of `tools/call`.
///
/// Per the MCP spec a *tool* failure is a successful JSON-RPC response with
/// `isError: true` — it is a result the model should see and react to, not a
/// transport error. Only protocol-level faults become JSON-RPC errors.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CallToolResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "structuredContent", default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl CallToolResult {
    pub fn text(msg: impl Into<String>) -> Self {
        Self { content: vec![Content::text(msg)], is_error: None, structured_content: None }
    }

    /// A structured result: pretty JSON for the model to read, plus the raw
    /// value in `structuredContent` for clients that can consume it.
    pub fn structured(value: Value) -> Self {
        let rendered = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Self {
            content: vec![Content::text(rendered)],
            is_error: None,
            structured_content: Some(value),
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self { content: vec![Content::text(msg)], is_error: Some(true), structured_content: None }
    }

    pub fn is_failure(&self) -> bool {
        self.is_error.unwrap_or(false)
    }

    /// Infallible serialization: the type has no map keys that can fail to
    /// encode, so a failure here can only be an allocator problem.
    pub fn into_value(self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|err| {
            json!({
                "content": [{ "type": "text", "text": format!("result serialization failed: {err}") }],
                "isError": true
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_preserves_unknown_upstream_fields() {
        let raw = json!({
            "name": "search",
            "description": "d",
            "inputSchema": { "type": "object" },
            "annotations": { "readOnlyHint": true },
            "title": "Search"
        });
        let tool: Tool = serde_json::from_value(raw).unwrap();
        assert!(tool.extra.contains_key("annotations"));

        let round_tripped = serde_json::to_value(&tool).unwrap();
        assert_eq!(round_tripped["annotations"]["readOnlyHint"], json!(true));
        assert_eq!(round_tripped["title"], json!("Search"));
    }

    #[test]
    fn tool_without_input_schema_gets_an_empty_object_schema() {
        let tool: Tool = serde_json::from_value(json!({ "name": "t" })).unwrap();
        assert_eq!(tool.input_schema["type"], json!("object"));
    }

    #[test]
    fn error_result_is_flagged() {
        assert!(CallToolResult::error("boom").is_failure());
        assert!(!CallToolResult::text("ok").is_failure());
    }

    #[test]
    fn structured_result_carries_both_renderings() {
        let result = CallToolResult::structured(json!({ "count": 2 }));
        assert_eq!(result.structured_content, Some(json!({ "count": 2 })));
        let Content::Text { text } = &result.content[0] else { panic!("expected text block") };
        assert!(text.contains("\"count\": 2"));
    }

    #[test]
    fn unknown_content_blocks_round_trip() {
        let raw = json!({ "content": [{ "type": "image", "data": "x", "mimeType": "image/png" }] });
        let result: CallToolResult = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(result).unwrap()["content"], raw["content"]);
    }
}
