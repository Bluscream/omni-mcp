use serde_json::json;

use super::*;

fn backend() -> NativeBackend {
    NativeBackend::with_defaults(&ToolPolicy::default())
}

#[tokio::test]
async fn every_embedded_crate_contributes_its_tools() {
    let tools = backend().list_tools(Duration::from_secs(1)).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    for expected in [
        "diff_text",
        "diff_json",
        "regex_match",
        "count_stats",
        "grep_search",
        "hex_view",
        "hex_patch",
        "eval_code",
        "read_resx",
        "write_resx_entry",
        "everything_search",
    ] {
        assert!(names.contains(&expected), "{expected} missing from {names:?}");
    }
}

#[test]
fn no_two_crates_claim_the_same_tool_name() {
    let backend = backend();
    assert!(backend.collisions().is_empty(), "unreachable tools: {:?}", backend.collisions());
}

#[tokio::test]
async fn a_call_reaches_the_crate_that_owns_the_tool() {
    let result = backend()
        .call("count_stats", json!({ "text": "a b c" }), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.structured_content.unwrap()["words"], 3);
}

#[tokio::test]
async fn an_unknown_tool_is_not_found() {
    let err = backend().call("no_such_tool", json!({}), Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(err, ToolError::NotFound(_)));
}

#[tokio::test]
async fn a_policy_denial_survives_the_error_mapping() {
    // Code execution is off by default, so this exercises Denied crossing the
    // crate boundary rather than being flattened into a generic failure.
    let err = backend()
        .call("eval_code", json!({ "language": "sh", "code": "id" }), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
}

#[tokio::test]
async fn a_bad_argument_survives_the_error_mapping() {
    let err = backend().call("count_stats", json!({}), Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)), "got {err:?}");
}

#[tokio::test]
async fn ssh_tools_appear_only_when_servers_are_configured() {
    let without = backend().list_tools(Duration::from_secs(1)).await.unwrap();
    assert!(!without.iter().any(|t| t.name == "ssh_execute"));

    let config = Config {
        ssh: vec![SshServerConfig {
            name: "box".into(),
            host: "127.0.0.1".into(),
            port: 22,
            user: "someone".into(),
            password: Some("pw".into()),
            private_key: None,
            passphrase: None,
            fingerprint: None,
            socks_proxy: None,
            whitelist: Vec::new(),
            blacklist: Vec::new(),
            bypass_allowed_roots: false,
            enabled: true,
        }],
        ..Default::default()
    };
    let with =
        NativeBackend::from_config(&config).list_tools(Duration::from_secs(1)).await.unwrap();
    assert!(with.iter().any(|t| t.name == "ssh_execute"));
}

#[tokio::test]
async fn status_is_ready_and_never_spawns() {
    assert_eq!(backend().status().await, BackendStatus::Ready);
}
