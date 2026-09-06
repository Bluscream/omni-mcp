//! Binary inspection and patching.
//!
//! Fixes carried over from the previous implementation:
//! * `hex_view` returned one unbroken hex string with no ASCII gutter and read
//!   the whole file into memory regardless of size.
//! * `hex_patch` advanced the cursor by the *replacement* length after a
//!   substitution, so a shorter replacement rescanned bytes it had just written
//!   and a longer one skipped over input. It also grew files silently when an
//!   offset write ran past the end, and never took a backup.

use std::io::{Read, Seek, SeekFrom};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{NativeTool, ToolContext, args, unknown};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};

pub struct HexTools;

const DEFAULT_VIEW_BYTES: u64 = 256;
const MAX_VIEW_BYTES: u64 = 1 << 20;
const BYTES_PER_ROW: usize = 16;

#[async_trait]
impl NativeTool for HexTools {
    fn descriptors(&self) -> Vec<Tool> {
        vec![
            Tool::new(
                "hex_view",
                "Dumps a byte range of a file as a classic hex view with offsets and an ASCII \
                 gutter. Reads only the requested window, so it is safe on very large files.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute file path" },
                        "offset": {
                            "type": "integer",
                            "description": "Byte offset to start at (default 0)"
                        },
                        "length": {
                            "type": "integer",
                            "description": "Bytes to read (default 256, maximum 1048576)"
                        }
                    },
                    "required": ["path"]
                }),
            ),
            Tool::new(
                "hex_patch",
                "Modifies a binary file, either by replacing every occurrence of a hex pattern \
                 or by writing hex bytes at an offset. Requires `apply: true` to write; \
                 otherwise it reports what would change.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute file path" },
                        "search_hex": {
                            "type": "string",
                            "description": "Hex byte pattern to find, e.g. \"48656c6c6f\""
                        },
                        "replace_hex": {
                            "type": "string",
                            "description": "Hex bytes to substitute for each match"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Byte offset for a direct write"
                        },
                        "hex_data": {
                            "type": "string",
                            "description": "Hex bytes to write at `offset`"
                        },
                        "apply": {
                            "type": "boolean",
                            "description": "Write to disk. Default false: report only."
                        },
                        "allow_resize": {
                            "type": "boolean",
                            "description": "Permit the patch to change the file's length \
                                            (default false; resizing usually corrupts binaries)"
                        }
                    },
                    "required": ["path"]
                }),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        let ctx = ctx.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || match name.as_str() {
            "hex_view" => view(&args, &ctx),
            "hex_patch" => patch(&args, &ctx),
            other => Err(unknown(other)),
        })
        .await
        .map_err(|e| ToolError::Failed(format!("hex task failed: {e}")))?
    }
}

fn view(arguments: &Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
    let path = ctx.resolve(args::string(arguments, "path")?)?;
    let offset = args::u64_or(arguments, "offset", 0)?;
    let requested = args::u64_or(arguments, "length", DEFAULT_VIEW_BYTES)?.min(MAX_VIEW_BYTES);

    let mut file = std::fs::File::open(&path)
        .map_err(|e| ToolError::Failed(format!("could not open {}: {e}", path.display())))?;
    let total = file
        .metadata()
        .map_err(|e| ToolError::Failed(format!("could not stat {}: {e}", path.display())))?
        .len();

    if offset > total {
        return Err(ToolError::InvalidArguments(format!(
            "offset {offset} is past the end of the {total}-byte file"
        )));
    }

    file.seek(SeekFrom::Start(offset))
        .map_err(|e| ToolError::Failed(format!("seek failed: {e}")))?;
    let to_read = requested.min(total - offset);
    let mut buffer = vec![0u8; usize::try_from(to_read).unwrap_or(0)];
    file.read_exact(&mut buffer).map_err(|e| ToolError::Failed(format!("read failed: {e}")))?;

    Ok(CallToolResult::structured(json!({
        "path": path.display().to_string(),
        "total_bytes": total,
        "offset": offset,
        "bytes_read": buffer.len(),
        "hex": hex::encode(&buffer),
        "dump": dump(&buffer, offset)
    })))
}

/// Renders `xxd`-style rows: offset, hex columns, printable ASCII.
fn dump(bytes: &[u8], base: u64) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for (row, chunk) in bytes.chunks(BYTES_PER_ROW).enumerate() {
        let address = base + (row * BYTES_PER_ROW) as u64;
        let _ = write!(out, "{address:08x}  ");
        for column in 0..BYTES_PER_ROW {
            match chunk.get(column) {
                Some(byte) => {
                    let _ = write!(out, "{byte:02x} ");
                }
                None => out.push_str("   "),
            }
            if column == BYTES_PER_ROW / 2 - 1 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for byte in chunk {
            out.push(if byte.is_ascii_graphic() || *byte == b' ' { *byte as char } else { '.' });
        }
        out.push_str("|\n");
    }
    out
}

fn patch(arguments: &Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
    let path = ctx.resolve(args::string(arguments, "path")?)?;
    let apply = args::bool_or(arguments, "apply", false)?;
    let allow_resize = args::bool_or(arguments, "allow_resize", false)?;
    if apply {
        ctx.require_file_mutation()?;
    }

    let original = std::fs::read(&path)
        .map_err(|e| ToolError::Failed(format!("could not read {}: {e}", path.display())))?;

    let (patched, summary) = if let Some(search) = args::opt_string(arguments, "search_hex")? {
        let replacement = args::opt_string(arguments, "replace_hex")?.unwrap_or("");
        substitute(&original, &decode(search, "search_hex")?, &decode(replacement, "replace_hex")?)?
    } else {
        let offset = args::opt_u64(arguments, "offset")?.ok_or_else(|| {
            ToolError::InvalidArguments(
                "supply either `search_hex` (with `replace_hex`) or `offset` with `hex_data`"
                    .into(),
            )
        })?;
        let data = decode(args::string(arguments, "hex_data")?, "hex_data")?;
        overwrite(&original, offset, &data)?
    };

    if patched.len() != original.len() && !allow_resize {
        return Err(ToolError::InvalidArguments(format!(
            "this patch would change the file size from {} to {} bytes, which usually corrupts a \
             binary; pass allow_resize: true if that is intended",
            original.len(),
            patched.len()
        )));
    }

    if apply {
        // Keep a copy: a bad byte patch is otherwise unrecoverable.
        let backup = path.with_extension(format!(
            "{}.omni-bak",
            path.extension().and_then(|e| e.to_str()).unwrap_or("bin")
        ));
        std::fs::write(&backup, &original)
            .map_err(|e| ToolError::Failed(format!("could not write backup: {e}")))?;
        std::fs::write(&path, &patched)
            .map_err(|e| ToolError::Failed(format!("could not write {}: {e}", path.display())))?;

        return Ok(CallToolResult::structured(json!({
            "path": path.display().to_string(),
            "applied": true,
            "backup": backup.display().to_string(),
            "original_bytes": original.len(),
            "new_bytes": patched.len(),
            "summary": summary
        })));
    }

    Ok(CallToolResult::structured(json!({
        "path": path.display().to_string(),
        "applied": false,
        "original_bytes": original.len(),
        "new_bytes": patched.len(),
        "summary": format!("{summary} (preview only; pass apply: true to write)")
    })))
}

/// Replaces every non-overlapping occurrence of `needle`, scanning the
/// *original* buffer so replacement bytes are never rescanned.
fn substitute(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> ToolResult<(Vec<u8>, String)> {
    if needle.is_empty() {
        return Err(ToolError::InvalidArguments("`search_hex` must not be empty".into()));
    }

    let mut out = Vec::with_capacity(haystack.len());
    let mut cursor = 0;
    let mut count = 0u64;

    for position in memchr::memmem::find_iter(haystack, needle) {
        if position < cursor {
            continue; // overlaps a previous match
        }
        out.extend_from_slice(&haystack[cursor..position]);
        out.extend_from_slice(replacement);
        cursor = position + needle.len();
        count += 1;
    }
    out.extend_from_slice(&haystack[cursor..]);

    Ok((out, format!("replaced {count} occurrence(s)")))
}

fn overwrite(original: &[u8], offset: u64, data: &[u8]) -> ToolResult<(Vec<u8>, String)> {
    let start = usize::try_from(offset)
        .map_err(|_| ToolError::InvalidArguments(format!("offset {offset} is too large")))?;
    if start > original.len() {
        return Err(ToolError::InvalidArguments(format!(
            "offset {start} is past the end of the {}-byte file",
            original.len()
        )));
    }

    let mut out = original.to_vec();
    let end = start + data.len();
    if end > out.len() {
        out.resize(end, 0);
    }
    out[start..end].copy_from_slice(data);
    Ok((out, format!("wrote {} byte(s) at offset {start}", data.len())))
}

fn decode(text: &str, field: &str) -> ToolResult<Vec<u8>> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace() && *c != ':').collect();
    hex::decode(&cleaned)
        .map_err(|e| ToolError::InvalidArguments(format!("{field:?} is not valid hex: {e}")))
}

#[cfg(test)]
mod tests;
