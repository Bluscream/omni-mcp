use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::config::ToolPolicy;

const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root>
  <resheader name="version">
    <value>2.0</value>
  </resheader>
  <data name="Greeting" xml:space="preserve">
    <value>Hello</value>
    <comment>Shown on the home screen</comment>
  </data>
  <data name="Farewell" xml:space="preserve">
    <value>Bye</value>
  </data>
</root>
"#;

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

    fn write(&self, name: &str, content: &str) -> String {
        let path = self.root.join(name);
        std::fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap()
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
    let result = ResxTools.call(name, arguments, ctx).await?;
    Ok(result.structured_content.expect("resx tools return structured results"))
}

#[test]
fn parsing_separates_values_from_comments() {
    let entries = parse(SAMPLE).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].key, "Greeting");
    assert_eq!(entries[0].value, "Hello");
    assert_eq!(entries[0].comment.as_deref(), Some("Shown on the home screen"));
    assert_eq!(entries[1].comment, None);
}

#[test]
fn resheader_elements_are_not_treated_as_entries() {
    // The old reader keyed off any text inside the document and captured these.
    let entries = parse(SAMPLE).unwrap();
    assert!(entries.iter().all(|e| e.key != "version"));
}

#[test]
fn escaped_values_are_decoded() {
    let xml = r#"<root><data name="K"><value>a &amp; b &lt;tag&gt;</value></data></root>"#;
    assert_eq!(parse(xml).unwrap()[0].value, "a & b <tag>");
}

#[test]
fn cdata_values_are_read() {
    let xml = r#"<root><data name="K"><value><![CDATA[raw <stuff>]]></value></data></root>"#;
    assert_eq!(parse(xml).unwrap()[0].value, "raw <stuff>");
}

#[test]
fn malformed_xml_is_reported_rather_than_silently_truncated() {
    assert!(parse("<root><data name=\"K\"><value>x</wrong></root>").is_err());
}

#[test]
fn updating_a_key_changes_only_its_value() {
    let (updated, created) = upsert(SAMPLE, "Greeting", "Hi there", None).unwrap();
    assert!(!created);

    let entries = parse(&updated).unwrap();
    assert_eq!(entries[0].value, "Hi there");
    // The comment must survive; the old writer replaced it too.
    assert_eq!(entries[0].comment.as_deref(), Some("Shown on the home screen"));
    assert_eq!(entries[1].value, "Bye");
}

#[test]
fn adding_a_key_produces_a_real_element_not_escaped_text() {
    let (updated, created) = upsert(SAMPLE, "NewKey", "New value", None).unwrap();
    assert!(created);

    // This is the exact defect in the previous implementation.
    assert!(!updated.contains("&lt;data"), "entry was written as escaped text:\n{updated}");
    assert!(updated.contains(r#"<data name="NewKey""#));

    let entries = parse(&updated).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].key, "NewKey");
    assert_eq!(entries[2].value, "New value");
}

#[test]
fn a_new_entry_may_carry_a_comment() {
    let (updated, _) = upsert(SAMPLE, "K", "V", Some("why this exists")).unwrap();
    let entry = parse(&updated).unwrap().into_iter().find(|e| e.key == "K").unwrap();
    assert_eq!(entry.comment.as_deref(), Some("why this exists"));
}

#[test]
fn values_needing_escaping_round_trip_exactly() {
    let tricky = r#"a & b < c > d "quoted" 'single'"#;
    let (updated, _) = upsert(SAMPLE, "Tricky", tricky, None).unwrap();

    assert!(updated.contains("&amp;"), "ampersand was not escaped");
    let entry = parse(&updated).unwrap().into_iter().find(|e| e.key == "Tricky").unwrap();
    assert_eq!(entry.value, tricky);
}

#[test]
fn a_key_containing_a_quote_is_escaped_in_the_attribute() {
    let key = r#"weird"key"#;
    let (updated, _) = upsert(SAMPLE, key, "v", None).unwrap();
    let entry = parse(&updated).unwrap().into_iter().find(|e| e.key == key);
    assert!(entry.is_some(), "key with a quote did not survive:\n{updated}");
}

#[test]
fn repeated_upserts_are_idempotent() {
    let (once, _) = upsert(SAMPLE, "K", "V", None).unwrap();
    let (twice, created) = upsert(&once, "K", "V", None).unwrap();
    assert!(!created);
    assert_eq!(parse(&once).unwrap().len(), parse(&twice).unwrap().len());
}

#[test]
fn the_xml_declaration_and_untouched_entries_are_preserved() {
    let (updated, _) = upsert(SAMPLE, "Greeting", "Hi", None).unwrap();
    assert!(updated.starts_with(r#"<?xml version="1.0" encoding="utf-8"?>"#));
    assert!(updated.contains(r#"<resheader name="version">"#));
}

#[tokio::test]
async fn reading_a_file_returns_keyed_entries() {
    let fixture = Fixture::new();
    let path = fixture.write("Strings.resx", SAMPLE);

    let out = run("read_resx", json!({ "path": path }), &fixture.ctx(false)).await.unwrap();
    assert_eq!(out["count"], 2);
    assert_eq!(out["entries"]["Greeting"]["value"], "Hello");
    assert_eq!(out["entries"]["Farewell"].get("comment"), None);
}

#[tokio::test]
async fn writing_a_missing_file_creates_a_valid_resx() {
    let fixture = Fixture::new();
    let path = fixture.root.join("New.resx").display().to_string();

    let out = run(
        "write_resx_entry",
        json!({ "path": path, "key": "First", "value": "Value" }),
        &fixture.ctx(true),
    )
    .await
    .unwrap();

    assert_eq!(out["created"], json!(true));
    let content = fixture.read("New.resx");
    assert!(content.contains("text/microsoft-resx"), "header missing:\n{content}");
    assert_eq!(parse(&content).unwrap()[0].value, "Value");
}

#[tokio::test]
async fn writing_requires_the_file_mutation_capability() {
    let fixture = Fixture::new();
    let path = fixture.write("Strings.resx", SAMPLE);

    let err = run(
        "write_resx_entry",
        json!({ "path": path, "key": "K", "value": "V" }),
        &fixture.ctx(false),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ToolError::Denied(_)));
    assert_eq!(fixture.read("Strings.resx"), SAMPLE);
}

#[tokio::test]
async fn an_empty_key_is_rejected() {
    let fixture = Fixture::new();
    let path = fixture.write("Strings.resx", SAMPLE);
    let err = run(
        "write_resx_entry",
        json!({ "path": path, "key": "  ", "value": "V" }),
        &fixture.ctx(true),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn a_path_outside_the_allowed_roots_is_denied() {
    let fixture = Fixture::new();
    let err = run("read_resx", json!({ "path": "/etc/hostname" }), &fixture.ctx(false))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)));
}
