use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use crate::traits::McpModule;
use crate::types::{CallToolResult, Tool};

pub struct CommonModule;

impl CommonModule {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpModule for CommonModule {
    fn name(&self) -> &'static str {
        "common"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "regex_match".to_string(),
                description: Some("Searches text for matches of a regex pattern".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern" },
                        "text": { "type": "string", "description": "Text to search" }
                    },
                    "required": ["pattern", "text"]
                }),
            },
            Tool {
                name: "count_stats".to_string(),
                description: Some("Counts lines, words, characters, and bytes in text".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Text to analyze" }
                    },
                    "required": ["text"]
                }),
            },
        ]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        match name {
            "regex_match" => {
                let pattern = arguments.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let text = arguments.get("text").and_then(|v| v.as_str()).unwrap_or("");

                let re = Regex::new(pattern).map_err(|e| format!("Invalid regex: {}", e))?;
                let matches: Vec<String> = re.find_iter(text).map(|m| m.as_str().to_string()).collect();

                let res = json!({
                    "count": matches.len(),
                    "matches": matches
                });

                Ok(CallToolResult::text(serde_json::to_string_pretty(&res).unwrap()))
            }
            "count_stats" => {
                let text = arguments.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let lines = text.lines().count();
                let words = text.split_whitespace().count();
                let chars = text.chars().count();
                let bytes = text.len();

                let res = json!({
                    "lines": lines,
                    "words": words,
                    "chars": chars,
                    "bytes": bytes
                });

                Ok(CallToolResult::text(serde_json::to_string_pretty(&res).unwrap()))
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }
}
