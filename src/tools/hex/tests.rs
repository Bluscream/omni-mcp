use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::config::ToolPolicy;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }

    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> String {
        let path = self.root.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.display().to_string()
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.root.join(name)).unwrap()
    }

    fn ctx(&self, mutable: bool) -> ToolContext {
        ToolContext::new(ToolPolicy {
            allow_file_mutation: mutable,
            allowed_roots: vec![self.root.clone()],
            ..Default::default()
        })
    }
}

async fn run(name: &str, arguments: Value, ctx: &ToolContext) -> ToolResult<Value> {
    let result = HexTools.call(name, arguments, ctx).await?;
    Ok(result.structured_content.expect("hex tools return structured results"))
}

#[tokio::test]
async fn view_returns_hex_and_an_ascii_dump() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"Hello\x00World");

    let out = run("hex_view", json!({ "path": path }), &fixture.ctx(false)).await.unwrap();

    assert_eq!(out["total_bytes"], 11);
    assert_eq!(out["hex"], "48656c6c6f00576f726c64");
    let dump = out["dump"].as_str().unwrap();
    assert!(dump.contains("00000000"));
    assert!(dump.contains("|Hello.World|"), "{dump}");
}

#[tokio::test]
async fn view_honours_offset_and_length() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"0123456789");

    let out =
        run("hex_view", json!({ "path": path, "offset": 4, "length": 3 }), &fixture.ctx(false))
            .await
            .unwrap();

    assert_eq!(out["hex"], "343536");
    assert_eq!(out["bytes_read"], 3);
    assert_eq!(out["offset"], 4);
}

#[tokio::test]
async fn view_clamps_a_length_that_runs_past_the_end() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"abc");

    let out =
        run("hex_view", json!({ "path": path, "offset": 1, "length": 999 }), &fixture.ctx(false))
            .await
            .unwrap();
    assert_eq!(out["bytes_read"], 2);
}

#[tokio::test]
async fn view_rejects_an_offset_past_the_end() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"abc");
    let err = run("hex_view", json!({ "path": path, "offset": 99 }), &fixture.ctx(false))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn view_of_an_empty_file_succeeds() {
    let fixture = Fixture::new();
    let path = fixture.write("empty.bin", b"");
    let out = run("hex_view", json!({ "path": path }), &fixture.ctx(false)).await.unwrap();
    assert_eq!(out["bytes_read"], 0);
}

#[tokio::test]
async fn patch_previews_by_default_and_writes_only_with_apply() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"AAAA");

    let preview = run(
        "hex_patch",
        json!({ "path": &path, "search_hex": "41", "replace_hex": "42" }),
        &fixture.ctx(true),
    )
    .await
    .unwrap();
    assert_eq!(preview["applied"], json!(false));
    assert_eq!(fixture.read("f.bin"), b"AAAA");

    let applied = run(
        "hex_patch",
        json!({ "path": &path, "search_hex": "41", "replace_hex": "42", "apply": true }),
        &fixture.ctx(true),
    )
    .await
    .unwrap();
    assert_eq!(applied["applied"], json!(true));
    assert_eq!(fixture.read("f.bin"), b"BBBB");
}

#[tokio::test]
async fn applying_a_patch_leaves_a_backup() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"AAAA");

    let out = run(
        "hex_patch",
        json!({ "path": path, "search_hex": "41", "replace_hex": "42", "apply": true }),
        &fixture.ctx(true),
    )
    .await
    .unwrap();

    let backup = out["backup"].as_str().unwrap();
    assert_eq!(std::fs::read(backup).unwrap(), b"AAAA");
}

#[tokio::test]
async fn patching_requires_the_file_mutation_capability() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"AAAA");

    let err = run(
        "hex_patch",
        json!({ "path": path, "search_hex": "41", "replace_hex": "42", "apply": true }),
        &fixture.ctx(false),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ToolError::Denied(_)));
    assert_eq!(fixture.read("f.bin"), b"AAAA");
}

#[tokio::test]
async fn a_size_changing_patch_is_refused_unless_resize_is_allowed() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"AAAA");

    let err = run(
        "hex_patch",
        json!({ "path": &path, "search_hex": "41", "replace_hex": "4243" }),
        &fixture.ctx(true),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("change the file size"));

    let ok = run(
        "hex_patch",
        json!({ "path": &path, "search_hex": "41", "replace_hex": "4243", "allow_resize": true, "apply": true }),
        &fixture.ctx(true),
    )
    .await
    .unwrap();
    assert_eq!(ok["new_bytes"], 8);
    assert_eq!(fixture.read("f.bin"), b"BCBCBCBC");
}

#[tokio::test]
async fn offset_writes_replace_bytes_in_place() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"0123456789");

    run(
        "hex_patch",
        json!({ "path": path, "offset": 2, "hex_data": "ffee", "apply": true }),
        &fixture.ctx(true),
    )
    .await
    .unwrap();

    assert_eq!(fixture.read("f.bin"), b"01\xff\xee456789");
}

#[tokio::test]
async fn an_offset_write_past_the_end_is_rejected() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"abc");
    let err = run(
        "hex_patch",
        json!({ "path": path, "offset": 100, "hex_data": "ff" }),
        &fixture.ctx(true),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn a_patch_with_neither_form_of_argument_explains_what_is_needed() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"abc");
    let err = run("hex_patch", json!({ "path": path }), &fixture.ctx(true)).await.unwrap_err();
    assert!(err.to_string().contains("search_hex"));
}

#[tokio::test]
async fn malformed_hex_is_an_argument_error() {
    let fixture = Fixture::new();
    let path = fixture.write("f.bin", b"abc");
    for bad in ["zz", "abc"] {
        let err = run(
            "hex_patch",
            json!({ "path": &path, "offset": 0, "hex_data": bad }),
            &fixture.ctx(true),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "{bad} should be rejected");
    }
}

#[test]
fn hex_input_may_be_spaced_or_colon_separated() {
    assert_eq!(decode("48 65 6c", "f").unwrap(), b"Hel");
    assert_eq!(decode("48:65:6c", "f").unwrap(), b"Hel");
    assert_eq!(decode("", "f").unwrap(), Vec::<u8>::new());
}

#[test]
fn substitution_does_not_rescan_bytes_it_just_wrote() {
    // The old loop advanced by the replacement length, so replacing "A" with
    // "AA" looped forever and replacing "AA" with "A" skipped input.
    let (out, summary) = substitute(b"AAAA", b"A", b"AA").unwrap();
    assert_eq!(out, b"AAAAAAAA");
    assert!(summary.contains('4'));

    let (out, _) = substitute(b"AAAA", b"AA", b"A").unwrap();
    assert_eq!(out, b"AA");
}

#[test]
fn substitution_of_overlapping_patterns_is_non_overlapping() {
    let (out, _) = substitute(b"aaaa", b"aa", b"b").unwrap();
    assert_eq!(out, b"bb");
}

#[test]
fn deleting_a_pattern_is_expressed_as_an_empty_replacement() {
    let (out, _) = substitute(b"xAyAz", b"A", b"").unwrap();
    assert_eq!(out, b"xyz");
}

#[test]
fn an_empty_search_pattern_is_rejected() {
    assert!(substitute(b"abc", b"", b"x").is_err());
}

#[test]
fn a_pattern_that_is_absent_leaves_the_buffer_untouched() {
    let (out, summary) = substitute(b"abc", b"zz", b"y").unwrap();
    assert_eq!(out, b"abc");
    assert!(summary.contains('0'));
}

#[test]
fn the_dump_pads_a_short_final_row() {
    let rendered = dump(b"ab", 0);
    assert!(rendered.starts_with("00000000  61 62 "));
    assert!(rendered.trim_end().ends_with("|ab|"));
}
