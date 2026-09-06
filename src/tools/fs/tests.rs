use serde_json::json;

use super::*;
use crate::config::ToolPolicy;

fn mutable_ctx(root: &Path) -> ToolContext {
    ToolContext::new(ToolPolicy {
        allow_file_mutation: true,
        allowed_roots: vec![root.to_path_buf()],
        ..Default::default()
    })
}

fn readonly_ctx(root: &Path) -> ToolContext {
    ToolContext::new(ToolPolicy { allowed_roots: vec![root.to_path_buf()], ..Default::default() })
}

async fn search(arguments: Value, ctx: &ToolContext) -> ToolResult<Value> {
    let result = FsTools.call("grep_search", arguments, ctx).await?;
    Ok(result.structured_content.expect("search results are structured"))
}

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

    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.root.join(name)).unwrap()
    }

    fn path(&self) -> String {
        self.root.display().to_string()
    }
}

#[tokio::test]
async fn finds_literal_matches_with_line_numbers() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "alpha\nbeta\ngamma\n");

    let out =
        search(json!({ "path": fixture.path(), "pattern": "beta" }), &readonly_ctx(&fixture.root))
            .await
            .unwrap();

    assert_eq!(out["matches"], 1);
    assert_eq!(out["results"][0]["line"], 2);
    assert_eq!(out["results"][0]["text"], "beta");
}

#[tokio::test]
async fn replacement_is_a_preview_unless_apply_is_set() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "hello world\n");

    let out = search(
        json!({ "path": fixture.path(), "pattern": "world", "replace": "there" }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(out["applied"], json!(false));
    assert_eq!(out["results"][0]["replacement"], "hello there");
    assert!(!out["files_would_be_modified"].as_array().unwrap().is_empty());
    assert_eq!(fixture.read("a.txt"), b"hello world\n", "preview must not touch the file");
}

#[tokio::test]
async fn apply_writes_the_replacement() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "hello world\n");

    let out = search(
        json!({ "path": fixture.path(), "pattern": "world", "replace": "there", "apply": true }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(out["applied"], json!(true));
    assert_eq!(fixture.read("a.txt"), b"hello there\n");
}

#[tokio::test]
async fn crlf_line_endings_survive_a_replacement() {
    // The old implementation rejoined with "\n" and silently converted the file.
    let fixture = Fixture::new();
    fixture.write("win.txt", "one\r\ntwo\r\nthree\r\n");

    search(
        json!({ "path": fixture.path(), "pattern": "two", "replace": "TWO", "apply": true }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(fixture.read("win.txt"), b"one\r\nTWO\r\nthree\r\n");
}

#[tokio::test]
async fn a_missing_trailing_newline_is_not_invented() {
    // The old implementation appended one on every write.
    let fixture = Fixture::new();
    fixture.write("nonl.txt", "last line has no newline");

    search(
        json!({ "path": fixture.path(), "pattern": "last", "replace": "final", "apply": true }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(fixture.read("nonl.txt"), b"final line has no newline");
}

#[tokio::test]
async fn writing_requires_the_file_mutation_capability() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "hello\n");

    let err = search(
        json!({ "path": fixture.path(), "pattern": "hello", "replace": "x", "apply": true }),
        &readonly_ctx(&fixture.root),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
    assert_eq!(fixture.read("a.txt"), b"hello\n");
}

#[tokio::test]
async fn apply_without_replace_is_rejected() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "hello\n");

    let err = search(
        json!({ "path": fixture.path(), "pattern": "hello", "apply": true }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn binary_files_are_skipped_not_mangled() {
    let fixture = Fixture::new();
    fixture.write("blob.bin", [0x00, 0x01, b'h', b'i', 0x00]);
    fixture.write("a.txt", "hi\n");

    let out = search(
        json!({ "path": fixture.path(), "pattern": "hi", "replace": "yo", "apply": true }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(out["unreadable_or_binary_files_skipped"], 1);
    assert_eq!(fixture.read("blob.bin"), [0x00, 0x01, b'h', b'i', 0x00]);
    assert_eq!(fixture.read("a.txt"), b"yo\n");
}

#[tokio::test]
async fn gitignored_and_dot_git_files_are_not_searched() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "ignored.txt\n");
    fixture.write("ignored.txt", "secret\n");
    fixture.write(".git/config", "secret\n");
    fixture.write("tracked.txt", "secret\n");

    let out = search(
        json!({ "path": fixture.path(), "pattern": "secret" }),
        &readonly_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(out["matches"], 1);
    assert!(out["results"][0]["file"].as_str().unwrap().ends_with("tracked.txt"));
}

#[tokio::test]
async fn a_glob_restricts_which_files_are_searched() {
    let fixture = Fixture::new();
    fixture.write("a.rs", "target\n");
    fixture.write("b.txt", "target\n");

    let out = search(
        json!({ "path": fixture.path(), "pattern": "target", "glob": "*.rs" }),
        &readonly_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(out["matches"], 1);
    assert!(out["results"][0]["file"].as_str().unwrap().ends_with("a.rs"));
}

#[tokio::test]
async fn regex_mode_expands_capture_groups_in_the_replacement() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "key=value\n");

    search(
        json!({
            "path": fixture.path(),
            "pattern": r"(\w+)=(\w+)",
            "is_regex": true,
            "replace": "$2=$1",
            "apply": true
        }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(fixture.read("a.txt"), b"value=key\n");
}

#[tokio::test]
async fn case_insensitive_search_and_replace() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "Hello HELLO hello\n");

    search(
        json!({
            "path": fixture.path(),
            "pattern": "hello",
            "case_sensitive": false,
            "replace": "hi",
            "apply": true
        }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(fixture.read("a.txt"), b"hi hi hi\n");
}

#[tokio::test]
async fn results_are_capped_but_the_true_count_is_reported() {
    let fixture = Fixture::new();
    fixture.write("many.txt", "match\n".repeat(100));

    let out = search(
        json!({ "path": fixture.path(), "pattern": "match", "max_results": 10 }),
        &readonly_ctx(&fixture.root),
    )
    .await
    .unwrap();

    assert_eq!(out["matches"], 100);
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(out["results"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn a_path_outside_the_allowed_roots_is_denied() {
    let fixture = Fixture::new();
    let err = search(json!({ "path": "/etc", "pattern": "root" }), &readonly_ctx(&fixture.root))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)));
}

#[tokio::test]
async fn an_empty_pattern_is_rejected() {
    let fixture = Fixture::new();
    let err =
        search(json!({ "path": fixture.path(), "pattern": "" }), &readonly_ctx(&fixture.root))
            .await
            .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn a_nonexistent_path_is_an_argument_error() {
    let fixture = Fixture::new();
    let err = search(
        json!({ "path": fixture.root.join("absent").display().to_string(), "pattern": "x" }),
        &readonly_ctx(&fixture.root),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn file_permissions_are_preserved_across_an_atomic_write() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let path = fixture.write("script.sh", "echo old\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    search(
        json!({ "path": fixture.path(), "pattern": "old", "replace": "new", "apply": true }),
        &mutable_ctx(&fixture.root),
    )
    .await
    .unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "executable bit was dropped");
}

#[test]
fn case_insensitive_literal_replacement_handles_repeats_and_no_match() {
    assert_eq!(replace_ignore_case("AbcABCabc", "abc", "x"), "xxx");
    assert_eq!(replace_ignore_case("nothing here", "zzz", "x"), "nothing here");
    assert_eq!(replace_ignore_case("abc", "", "x"), "abc");
}
