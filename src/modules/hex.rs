use async_trait::async_trait;
use serde_json::{json, Value};
use std::fs;
use crate::traits::McpModule;
use crate::types::{CallToolResult, Tool};

pub struct HexModule;

impl HexModule {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpModule for HexModule {
    fn name(&self) -> &'static str {
        "hex"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "hex_view".to_string(),
                description: Some("Reads a file as hex bytes with optional offset and length".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path to read" },
                        "offset": { "type": "integer", "description": "Byte offset to start reading (default: 0)" },
                        "length": { "type": "integer", "description": "Number of bytes to read" }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "hex_patch".to_string(),
                description: Some("Patches hex bytes in a binary file at an offset or searches and replaces hex pattern".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path to patch" },
                        "search_hex": { "type": "string", "description": "Hex pattern to search for (e.g. '414243')" },
                        "replace_hex": { "type": "string", "description": "Hex pattern to replace with" },
                        "offset": { "type": "integer", "description": "Byte offset to write hex data" },
                        "hex_data": { "type": "string", "description": "Hex bytes to write" }
                    },
                    "required": ["path"]
                }),
            },
        ]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        match name {
            "hex_view" => {
                let path_str = arguments.get("path").and_then(|v| v.as_str()).ok_or("Missing path")?;
                let offset = arguments.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let length = arguments.get("length").and_then(|v| v.as_u64()).map(|l| l as usize);

                let buffer = fs::read(path_str).map_err(|e| format!("Failed to read file {}: {}", path_str, e))?;
                let total_len = buffer.len();

                if offset >= total_len {
                    return Err(format!("Offset {} is beyond file size {}", offset, total_len));
                }

                let end = length.map(|l| (offset + l).min(total_len)).unwrap_or(total_len);
                let slice = &buffer[offset..end];

                let hex_str = slice.iter().map(|b| format!("{:02x}", b)).collect::<Vec<String>>().join("");

                let res = json!({
                    "path": path_str,
                    "total_bytes": total_len,
                    "offset": offset,
                    "bytes_read": slice.len(),
                    "hex": hex_str
                });

                Ok(CallToolResult::text(serde_json::to_string_pretty(&res).unwrap()))
            }
            "hex_patch" => {
                let path_str = arguments.get("path").and_then(|v| v.as_str()).ok_or("Missing path")?;
                let mut buffer = fs::read(path_str).map_err(|e| format!("Failed to read file {}: {}", path_str, e))?;

                if let Some(search_hex) = arguments.get("search_hex").and_then(|v| v.as_str()) {
                    let replace_hex = arguments.get("replace_hex").and_then(|v| v.as_str()).unwrap_or("");
                    let search_bytes = parse_hex(search_hex)?;
                    let replace_bytes = parse_hex(replace_hex)?;

                    let mut count = 0;
                    let mut i = 0;
                    while i + search_bytes.len() <= buffer.len() {
                        if &buffer[i..i + search_bytes.len()] == search_bytes.as_slice() {
                            buffer.splice(i..i + search_bytes.len(), replace_bytes.iter().cloned());
                            count += 1;
                            i += replace_bytes.len();
                        } else {
                            i += 1;
                        }
                    }

                    fs::write(path_str, &buffer).map_err(|e| format!("Failed to write patched file: {}", e))?;
                    return Ok(CallToolResult::text(format!("Replaced {} occurrences of hex pattern in {}", count, path_str)));
                }

                if let Some(offset) = arguments.get("offset").and_then(|v| v.as_u64()) {
                    let hex_data = arguments.get("hex_data").and_then(|v| v.as_str()).ok_or("Missing hex_data")?;
                    let patch_bytes = parse_hex(hex_data)?;
                    let off = offset as usize;

                    if off + patch_bytes.len() > buffer.len() {
                        buffer.resize(off + patch_bytes.len(), 0);
                    }

                    buffer[off..off + patch_bytes.len()].copy_from_slice(&patch_bytes);
                    fs::write(path_str, &buffer).map_err(|e| format!("Failed to write file: {}", e))?;
                    return Ok(CallToolResult::text(format!("Patched {} bytes at offset {} in {}", patch_bytes.len(), off, path_str)));
                }

                Err("Either (search_hex, replace_hex) or (offset, hex_data) must be provided".to_string())
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }
}

fn parse_hex(hex_str: &str) -> Result<Vec<u8>, String> {
    let clean = hex_str.replace([' ', '\n', '\r'], "");
    if !clean.len().is_multiple_of(2) {
        return Err("Hex string length must be even".to_string());
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(|e| format!("Invalid hex byte: {}", e)))
        .collect()
}
