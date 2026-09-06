use async_trait::async_trait;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use crate::traits::McpModule;
use crate::types::{CallToolResult, Tool};

pub struct DiffModule;

impl DiffModule {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpModule for DiffModule {
    fn name(&self) -> &'static str {
        "diff"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "diff_text".to_string(),
                description: Some("Computes a line-by-line diff between two text strings".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "old_text": { "type": "string", "description": "Original text" },
                        "new_text": { "type": "string", "description": "Modified text" }
                    },
                    "required": ["old_text", "new_text"]
                }),
            },
            Tool {
                name: "diff_json".to_string(),
                description: Some("Compares two JSON values and highlights structural differences".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "old_json": { "description": "Original JSON object or string" },
                        "new_json": { "description": "Modified JSON object or string" }
                    },
                    "required": ["old_json", "new_json"]
                }),
            },
        ]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        match name {
            "diff_text" => {
                let old_text = arguments.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
                let new_text = arguments.get("new_text").and_then(|v| v.as_str()).unwrap_or("");

                let diff = TextDiff::from_lines(old_text, new_text);
                let mut output = String::new();

                for change in diff.iter_all_changes() {
                    let sign = match change.tag() {
                        ChangeTag::Delete => "-",
                        ChangeTag::Insert => "+",
                        ChangeTag::Equal => " ",
                    };
                    output.push_str(&format!("{}{}", sign, change));
                }

                Ok(CallToolResult::text(output))
            }
            "diff_json" => {
                let old_val = arguments.get("old_json").unwrap_or(&Value::Null);
                let new_val = arguments.get("new_json").unwrap_or(&Value::Null);

                let old_str = serde_json::to_string_pretty(old_val).unwrap_or_default();
                let new_str = serde_json::to_string_pretty(new_val).unwrap_or_default();

                let diff = TextDiff::from_lines(&old_str, &new_str);
                let mut output = String::new();

                for change in diff.iter_all_changes() {
                    let sign = match change.tag() {
                        ChangeTag::Delete => "-",
                        ChangeTag::Insert => "+",
                        ChangeTag::Equal => " ",
                    };
                    output.push_str(&format!("{}{}", sign, change));
                }

                Ok(CallToolResult::text(output))
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }
}
