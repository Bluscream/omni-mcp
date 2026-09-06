//! .NET `.resx` resource files.
//!
//! The previous `write_resx_entry` was broken for new keys: it built the
//! element as a string and wrote it through `BytesText`, which XML-escapes its
//! input, so the file gained a line of literal `&lt;data name="..."&gt;` text
//! instead of an element. It also overwrote the text of `<comment>` elements
//! when updating a key, and never escaped the value. Both operations are now
//! event-based and structurally correct.

use async_trait::async_trait;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use serde_json::{Value, json};

use super::{NativeTool, ToolContext, args, unknown};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};

pub struct ResxTools;

/// Header of a fresh `.resx`, including the schema block Visual Studio expects.
const TEMPLATE: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
    "<root>\n",
    "  <resheader name=\"resmimetype\">\n",
    "    <value>text/microsoft-resx</value>\n",
    "  </resheader>\n",
    "  <resheader name=\"version\">\n",
    "    <value>2.0</value>\n",
    "  </resheader>\n",
    "</root>\n"
);

#[async_trait]
impl NativeTool for ResxTools {
    fn descriptors(&self) -> Vec<Tool> {
        vec![
            Tool::new(
                "read_resx",
                "Reads the string entries of a .NET .resx resource file, returning each key with \
                 its value and comment.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute path to the .resx file" }
                    },
                    "required": ["path"]
                }),
            ),
            Tool::new(
                "write_resx_entry",
                "Adds or updates one string entry in a .NET .resx file, preserving the rest of \
                 the document. Creates the file with a valid header if it does not exist.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute path to the .resx file" },
                        "key": { "type": "string", "description": "Resource key" },
                        "value": { "type": "string", "description": "Resource value" },
                        "comment": { "type": "string", "description": "Optional translator comment" }
                    },
                    "required": ["path", "key", "value"]
                }),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        let ctx = ctx.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || match name.as_str() {
            "read_resx" => read(&args, &ctx),
            "write_resx_entry" => write(&args, &ctx),
            other => Err(unknown(other)),
        })
        .await
        .map_err(|e| ToolError::Failed(format!("resx task failed: {e}")))?
    }
}

fn read(arguments: &Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
    let path = ctx.resolve(args::string(arguments, "path")?)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| ToolError::Failed(format!("could not read {}: {e}", path.display())))?;

    let entries = parse(&content)?;
    let rendered: serde_json::Map<String, Value> = entries
        .iter()
        .map(|entry| {
            let mut fields = serde_json::Map::new();
            fields.insert("value".into(), json!(entry.value));
            if let Some(comment) = &entry.comment {
                fields.insert("comment".into(), json!(comment));
            }
            (entry.key.clone(), Value::Object(fields))
        })
        .collect();

    Ok(CallToolResult::structured(json!({
        "path": path.display().to_string(),
        "count": entries.len(),
        "entries": rendered
    })))
}

#[derive(Debug, PartialEq)]
struct Entry {
    key: String,
    value: String,
    comment: Option<String>,
}

/// Extracts `<data name="…"><value>…</value></data>` entries. Text belonging to
/// `<comment>` is kept separate rather than overwriting the value, and
/// `<resheader>` elements are ignored.
fn parse(xml: &str) -> ToolResult<Vec<Entry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    let mut field: Option<Field> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => match tag.name().as_ref() {
                b"data" => current = attribute(&tag, b"name")?.map(new_entry),
                b"value" if current.is_some() => field = Some(Field::Value),
                b"comment" if current.is_some() => field = Some(Field::Comment),
                _ => {}
            },
            Ok(Event::Text(text)) => {
                if let (Some(entry), Some(active)) = (current.as_mut(), field) {
                    let decoded = text
                        .unescape()
                        .map_err(|e| ToolError::Failed(format!("malformed XML text: {e}")))?;
                    match active {
                        Field::Value => entry.value.push_str(&decoded),
                        Field::Comment => {
                            entry.comment.get_or_insert_with(String::new).push_str(&decoded);
                        }
                    }
                }
            }
            Ok(Event::CData(cdata)) => {
                if let (Some(entry), Some(Field::Value)) = (current.as_mut(), field) {
                    entry.value.push_str(&String::from_utf8_lossy(&cdata));
                }
            }
            Ok(Event::End(tag)) => match tag.name().as_ref() {
                b"value" | b"comment" => field = None,
                b"data" => {
                    if let Some(entry) = current.take() {
                        entries.push(entry);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ToolError::Failed(format!(
                    "invalid XML at byte {}: {e}",
                    reader.buffer_position()
                )));
            }
            _ => {}
        }
    }

    Ok(entries)
}

#[derive(Clone, Copy)]
enum Field {
    Value,
    Comment,
}

fn new_entry(key: String) -> Entry {
    Entry { key, value: String::new(), comment: None }
}

fn attribute(tag: &BytesStart<'_>, wanted: &[u8]) -> ToolResult<Option<String>> {
    for attribute in tag.attributes().flatten() {
        if attribute.key.as_ref() == wanted {
            let decoded = attribute
                .unescape_value()
                .map_err(|e| ToolError::Failed(format!("malformed attribute: {e}")))?;
            return Ok(Some(decoded.into_owned()));
        }
    }
    Ok(None)
}

fn write(arguments: &Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
    let path = ctx.resolve(args::string(arguments, "path")?)?;
    let key = args::string(arguments, "key")?;
    let value = args::string(arguments, "value")?;
    let comment = args::opt_string(arguments, "comment")?;
    ctx.require_file_mutation()?;

    if key.trim().is_empty() {
        return Err(ToolError::InvalidArguments("`key` must not be empty".into()));
    }

    let existing = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(e) => return Err(ToolError::Failed(format!("could not read {}: {e}", path.display()))),
    };

    let (updated, created) = upsert(&existing, key, value, comment)?;
    std::fs::write(&path, &updated)
        .map_err(|e| ToolError::Failed(format!("could not write {}: {e}", path.display())))?;

    Ok(CallToolResult::structured(json!({
        "path": path.display().to_string(),
        "key": key,
        "created": created,
        "action": if created { "added" } else { "updated" }
    })))
}

/// Returns the rewritten document and whether the key was newly created.
fn upsert(xml: &str, key: &str, value: &str, comment: Option<&str>) -> ToolResult<(String, bool)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());

    let mut in_target = false;
    let mut in_target_value = false;
    let mut wrote_value = false;
    let mut found = false;
    let mut depth = 0usize;

    let emit = |writer: &mut Writer<Vec<u8>>, event: Event<'_>| -> ToolResult<()> {
        writer
            .write_event(event)
            .map_err(|e| ToolError::Failed(format!("could not write XML: {e}")))
    };

    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => {
                depth += 1;
                match tag.name().as_ref() {
                    b"data" if attribute(&tag, b"name")?.as_deref() == Some(key) => {
                        in_target = true;
                        found = true;
                    }
                    b"value" if in_target => {
                        in_target_value = true;
                        emit(&mut writer, Event::Start(tag.clone()))?;
                        emit(&mut writer, Event::Text(BytesText::new(value)))?;
                        wrote_value = true;
                        continue;
                    }
                    _ => {}
                }
                emit(&mut writer, Event::Start(tag.into_owned()))?;
            }
            Ok(Event::Text(text)) => {
                // Suppress only the target `<value>`'s own text; comments and
                // whitespace elsewhere are passed through untouched.
                if !in_target_value {
                    emit(&mut writer, Event::Text(text.into_owned()))?;
                }
            }
            Ok(Event::End(tag)) => {
                depth = depth.saturating_sub(1);
                match tag.name().as_ref() {
                    b"value" if in_target_value => in_target_value = false,
                    b"data" if in_target => in_target = false,
                    b"root" if depth == 0 && !found => {
                        // Append the new entry just before `</root>`, as real
                        // elements rather than escaped text.
                        write_entry(&mut writer, key, value, comment)?;
                    }
                    _ => {}
                }
                emit(&mut writer, Event::End(tag.into_owned()))?;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ToolError::Failed(format!(
                    "invalid XML at byte {}: {e}",
                    reader.buffer_position()
                )));
            }
            Ok(other) => emit(&mut writer, other.into_owned())?,
        }
    }

    if found && !wrote_value {
        return Err(ToolError::Failed(format!(
            "entry {key:?} exists but has no <value> element; the file may be malformed"
        )));
    }

    let bytes = writer.into_inner();
    let rendered = String::from_utf8(bytes)
        .map_err(|e| ToolError::Failed(format!("resx output is not valid UTF-8: {e}")))?;
    Ok((rendered, !found))
}

fn write_entry(
    writer: &mut Writer<Vec<u8>>,
    key: &str,
    value: &str,
    comment: Option<&str>,
) -> ToolResult<()> {
    let fail = |e: std::io::Error| ToolError::Failed(format!("could not write XML: {e}"));

    let mut data = BytesStart::new("data");
    data.push_attribute(("name", key));
    data.push_attribute(("xml:space", "preserve"));

    writer.write_event(Event::Text(BytesText::from_escaped("  "))).map_err(fail)?;
    writer.write_event(Event::Start(data)).map_err(fail)?;
    writer.write_event(Event::Text(BytesText::from_escaped("\n    "))).map_err(fail)?;

    writer.write_event(Event::Start(BytesStart::new("value"))).map_err(fail)?;
    writer.write_event(Event::Text(BytesText::new(value))).map_err(fail)?;
    writer.write_event(Event::End(BytesEnd::new("value"))).map_err(fail)?;

    if let Some(comment) = comment {
        writer.write_event(Event::Text(BytesText::from_escaped("\n    "))).map_err(fail)?;
        writer.write_event(Event::Start(BytesStart::new("comment"))).map_err(fail)?;
        writer.write_event(Event::Text(BytesText::new(comment))).map_err(fail)?;
        writer.write_event(Event::End(BytesEnd::new("comment"))).map_err(fail)?;
    }

    writer.write_event(Event::Text(BytesText::from_escaped("\n  "))).map_err(fail)?;
    writer.write_event(Event::End(BytesEnd::new("data"))).map_err(fail)?;
    writer.write_event(Event::Text(BytesText::from_escaped("\n"))).map_err(fail)?;
    Ok(())
}

#[cfg(test)]
mod tests;
