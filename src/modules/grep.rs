use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

use crate::traits::McpModule;
use crate::types::{CallToolResult, Tool};

pub struct GrepModule;

impl GrepModule {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpModule for GrepModule {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![Tool {
            name: "grep_search".to_string(),
            description: Some("Searches for string or regex pattern across files and optionally replaces matches".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Target file or directory path" },
                    "pattern": { "type": "string", "description": "Pattern to search for" },
                    "is_regex": { "type": "boolean", "description": "Whether pattern is regex (default: false)" },
                    "replace": { "type": "string", "description": "Optional string to replace matches with" },
                    "max_depth": { "type": "integer", "description": "Max directory depth to search (default: 5)" }
                },
                "required": ["path", "pattern"]
            }),
        }]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        if name != "grep_search" {
            return Err(format!("Unknown tool: {}", name));
        }

        let path_str = arguments.get("path").and_then(|v| v.as_str()).ok_or("Missing path")?;
        let pattern = arguments.get("pattern").and_then(|v| v.as_str()).ok_or("Missing pattern")?;
        let is_regex = arguments.get("is_regex").and_then(|v| v.as_bool()).unwrap_or(false);
        let replace = arguments.get("replace").and_then(|v| v.as_str());
        let max_depth = arguments.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

        let root_path = Path::new(path_str);
        if !root_path.exists() {
            return Err(format!("Path does not exist: {}", path_str));
        }

        let mut results = Vec::new();
        let mut matches_count = 0;

        let regex = if is_regex {
            Some(Regex::new(pattern).map_err(|e| format!("Invalid regex: {}", e))?)
        } else {
            None
        };

        let files = if root_path.is_file() {
            vec![root_path.to_path_buf()]
        } else {
            WalkDir::new(root_path)
                .max_depth(max_depth)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect()
        };

        for file in files {
            if let Ok(content) = fs::read_to_string(&file) {
                let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
                let mut modified = false;

                for (idx, line) in lines.iter_mut().enumerate() {
                    let is_match = match &regex {
                        Some(re) => re.is_match(line),
                        None => line.contains(pattern),
                    };

                    if is_match {
                        matches_count += 1;
                        let old_line = line.clone();

                        if let Some(rep) = replace {
                            let new_line = match &regex {
                                Some(re) => re.replace_all(line, rep).to_string(),
                                None => line.replace(pattern, rep),
                            };
                            if new_line != old_line {
                                *line = new_line.clone();
                                modified = true;
                                results.push(format!("{}:{}: {} -> {}", file.display(), idx + 1, old_line, new_line));
                                continue;
                            }
                        }
                        results.push(format!("{}:{}: {}", file.display(), idx + 1, old_line));
                    }
                }

                if modified {
                    let new_content = lines.join("\n");
                    let _ = fs::write(&file, new_content);
                }
            }
        }

        let summary = format!("Found {} matches.\n\n{}", matches_count, results.join("\n"));
        Ok(CallToolResult::text(summary))
    }
}
