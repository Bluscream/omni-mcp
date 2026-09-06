use async_trait::async_trait;
use quick_xml::events::{Event, BytesText};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use serde_json::{json, Value};
use std::fs;
use std::io::Cursor;
use crate::traits::McpModule;
use crate::types::{CallToolResult, Tool};

pub struct ResxModule;

impl ResxModule {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpModule for ResxModule {
    fn name(&self) -> &'static str {
        "resx"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "read_resx".to_string(),
                description: Some("Reads entries (key-value pairs) from a .NET .resx XML resource file".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute file path to the .resx file" }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "write_resx_entry".to_string(),
                description: Some("Adds or updates a string entry in a .NET .resx file".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute file path to the .resx file" },
                        "key": { "type": "string", "description": "Resource key name" },
                        "value": { "type": "string", "description": "Resource string value" }
                    },
                    "required": ["path", "key", "value"]
                }),
            },
        ]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        match name {
            "read_resx" => {
                let path = arguments.get("path").and_then(|v| v.as_str()).ok_or("Missing path parameter")?;
                let content = fs::read_to_string(path).map_err(|e| format!("Failed to read file {}: {}", path, e))?;

                let mut reader = Reader::from_str(&content);
                reader.config_mut().trim_text(true);

                let mut entries = json!({});
                let mut current_name = None;

                let mut buf = Vec::new();
                loop {
                    match reader.read_event_into(&mut buf) {
                        Ok(Event::Start(e)) if e.name().as_ref() == b"data" => {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"name" {
                                    current_name = Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                            }
                        }
                        Ok(Event::Text(e)) => {
                            if let Some(ref name) = current_name {
                                let val = e.unescape().unwrap_or_default().to_string();
                                entries[name] = json!(val);
                            }
                        }
                        Ok(Event::End(e)) if e.name().as_ref() == b"data" => {
                            current_name = None;
                        }
                        Ok(Event::Eof) => break,
                        Err(e) => return Err(format!("XML parse error at position {}: {:?}", reader.buffer_position(), e)),
                        _ => (),
                    }
                    buf.clear();
                }

                Ok(CallToolResult::text(serde_json::to_string_pretty(&entries).unwrap()))
            }
            "write_resx_entry" => {
                let path = arguments.get("path").and_then(|v| v.as_str()).ok_or("Missing path parameter")?;
                let key = arguments.get("key").and_then(|v| v.as_str()).ok_or("Missing key parameter")?;
                let val = arguments.get("value").and_then(|v| v.as_str()).ok_or("Missing value parameter")?;

                let content = if std::path::Path::new(path).exists() {
                    fs::read_to_string(path).unwrap_or_else(|_| resx_template())
                } else {
                    resx_template()
                };

                let mut reader = Reader::from_str(&content);
                let mut writer = Writer::new(Cursor::new(Vec::new()));
                let mut buf = Vec::new();

                let mut inside_target_data = false;
                let mut key_exists = false;

                loop {
                    match reader.read_event_into(&mut buf) {
                        Ok(Event::Start(e)) => {
                            if e.name().as_ref() == b"data" {
                                for attr in e.attributes().flatten() {
                                    if attr.key.as_ref() == b"name" && attr.value.as_ref() == key.as_bytes() {
                                        inside_target_data = true;
                                        key_exists = true;
                                    }
                                }
                            }
                            writer.write_event(Event::Start(e)).map_err(|e| e.to_string())?;
                        }
                        Ok(Event::Text(e)) => {
                            if inside_target_data {
                                writer.write_event(Event::Text(BytesText::new(val))).map_err(|e| e.to_string())?;
                            } else {
                                writer.write_event(Event::Text(e)).map_err(|e| e.to_string())?;
                            }
                        }
                        Ok(Event::End(e)) => {
                            if e.name().as_ref() == b"data" {
                                inside_target_data = false;
                            }
                            if e.name().as_ref() == b"root" && !key_exists {
                                // Add new key before root closes
                                let data_node = format!("<data name=\"{}\" xml:space=\"preserve\"><value>{}</value></data>", key, val);
                                writer.write_event(Event::Text(BytesText::new(&data_node))).map_err(|e| e.to_string())?;
                            }
                            writer.write_event(Event::End(e)).map_err(|e| e.to_string())?;
                        }
                        Ok(Event::Eof) => break,
                        Ok(other) => {
                            writer.write_event(other).map_err(|e| e.to_string())?;
                        }
                        Err(e) => return Err(format!("Error parsing XML: {}", e)),
                    }
                    buf.clear();
                }

                let result_bytes = writer.into_inner().into_inner();
                fs::write(path, result_bytes).map_err(|e| format!("Failed to save resx to {}: {}", path, e))?;

                Ok(CallToolResult::text(format!("Successfully wrote entry '{}' = '{}' to {}", key, val, path)))
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }
}

fn resx_template() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root>\n</root>".to_string()
}
