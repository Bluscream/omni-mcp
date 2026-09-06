use serde_json::json;

use super::*;
use crate::config::{Limits, ToolPolicy, duration::HumanDuration};

fn router(config: Config) -> Router {
    Router::build(config).expect("router builds from a valid config")
}

fn request(method: &str, params: Value) -> Request {
    Request {
        jsonrpc: Some("2.0".into()),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    }
}

async fn call(router: &Router, name: &str, arguments: Value) -> Response {
    router
        .handle(request("tools/call", json!({ "name": name, "arguments": arguments })))
        .await
        .expect("a request must be answered")
}

#[tokio::test]
async fn initialize_echoes_a_supported_protocol_version() {
    let response = router(Config::default())
        .handle(request("initialize", json!({ "protocolVersion": "2024-11-05" })))
        .await
        .unwrap();

    let result = response.result.unwrap();
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "omni-mcp");
    assert!(result["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn initialize_offers_our_latest_version_for_an_unknown_proposal() {
    let response = router(Config::default())
        .handle(request("initialize", json!({ "protocolVersion": "1999-01-01" })))
        .await
        .unwrap();
    assert_eq!(response.result.unwrap()["protocolVersion"], crate::protocol::version::LATEST);
}

#[tokio::test]
async fn notifications_are_never_answered() {
    let notification = Request {
        jsonrpc: Some("2.0".into()),
        id: None,
        method: "notifications/initialized".into(),
        params: None,
    };
    // The old server replied to notifications, which strict clients reject.
    assert!(router(Config::default()).handle(notification).await.is_none());
}

#[tokio::test]
async fn ping_and_the_empty_list_methods_answer_without_error() {
    let router = router(Config::default());
    for method in ["ping", "prompts/list", "resources/list", "resources/templates/list"] {
        let response = router.handle(request(method, json!({}))).await.unwrap();
        assert!(response.error.is_none(), "{method} returned an error");
    }
}

#[tokio::test]
async fn an_unknown_method_is_method_not_found() {
    let response =
        router(Config::default()).handle(request("does/not/exist", json!({}))).await.unwrap();
    assert_eq!(response.error.unwrap().code, code::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn tools_list_includes_omni_status_and_the_native_tools() {
    let tools = router(Config::default()).list_tools().await;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    assert!(names.contains(&"omni_status"));
    assert!(names.contains(&"diff_text"));
    assert!(names.contains(&"grep_search"));
}

#[tokio::test]
async fn every_advertised_tool_gains_an_optional_timeout_argument() {
    let tools = router(Config::default()).list_tools().await;
    let diff = tools.iter().find(|t| t.name == "diff_text").unwrap();
    assert_eq!(diff.input_schema["properties"]["timeout"]["type"], "integer");
}

#[tokio::test]
async fn omni_status_reports_the_native_backend_as_ready() {
    let response = call(&router(Config::default()), "omni_status", json!({})).await;
    let result = response.result.unwrap();
    let report: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();

    assert_eq!(report["backends"][0]["name"], "native");
    assert_eq!(report["backends"][0]["status"], "ready");
    assert!(report["tool_count"].as_u64().unwrap() > 5);
}

#[tokio::test]
async fn omni_status_does_not_start_lazy_sidecars() {
    // The old status report spawned every sidecar to answer, which is exactly
    // the process storm this project exists to avoid.
    let config = Config {
        sidecars: vec![crate::config::SidecarConfig {
            name: "never".into(),
            command: "sleep".into(),
            args: vec!["3600".into()],
            env: std::collections::BTreeMap::new(),
            cwd: None,
            lazy: true,
            prefix: None,
            startup_timeout: HumanDuration::millis(200),
            restart_on_failure: true,
            enabled: true,
        }],
        limits: Limits { discovery_timeout: HumanDuration::millis(300), ..Default::default() },
        ..Default::default()
    };

    let started = std::time::Instant::now();
    let response = call(&router(config), "omni_status", json!({})).await;
    assert!(response.error.is_none());
    assert!(started.elapsed() < Duration::from_secs(5), "status blocked on a sidecar");
}

#[tokio::test]
async fn a_native_tool_call_returns_its_result() {
    let response =
        call(&router(Config::default()), "count_stats", json!({ "text": "a b c" })).await;
    let result = response.result.unwrap();
    assert_eq!(result["structuredContent"]["words"], 3);
    assert!(result.get("isError").is_none());
}

#[tokio::test]
async fn an_unknown_tool_returns_method_not_found_without_contacting_any_backend() {
    // Previously this broadcast the arguments to every sidecar and proxy.
    let response = call(&router(Config::default()), "no_such_tool", json!({ "secret": "x" })).await;
    assert_eq!(response.error.unwrap().code, code::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn a_tool_failure_is_a_successful_response_carrying_is_error() {
    let response = call(
        &router(Config::default()),
        "eval_code",
        json!({
            "language": "sh",
            "code": "echo hi"
        }),
    )
    .await;

    // Code execution is disabled by default, so this is a policy denial.
    let result = response.result.expect("tool failures are results, not RPC errors");
    assert_eq!(result["isError"], json!(true));
    assert!(result["content"][0]["text"].as_str().unwrap().contains("allow_code_execution"));
}

#[tokio::test]
async fn a_bad_argument_is_an_rpc_error_not_a_silent_default() {
    let response = call(&router(Config::default()), "count_stats", json!({})).await;
    assert_eq!(response.error.unwrap().code, code::INVALID_PARAMS);
}

#[tokio::test]
async fn tools_call_without_params_or_name_is_rejected() {
    let router = router(Config::default());

    let no_params = router
        .handle(Request {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(1)),
            method: "tools/call".into(),
            params: None,
        })
        .await
        .unwrap();
    assert_eq!(no_params.error.unwrap().code, code::INVALID_PARAMS);

    let no_name = router.handle(request("tools/call", json!({ "arguments": {} }))).await.unwrap();
    assert_eq!(no_name.error.unwrap().code, code::INVALID_PARAMS);
}

#[tokio::test]
async fn the_injected_timeout_is_not_forwarded_to_the_tool() {
    // `count_stats` would reject an unexpected argument if it received one.
    let response =
        call(&router(Config::default()), "count_stats", json!({ "text": "x", "timeout": 5 })).await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(response.result.unwrap()["structuredContent"]["chars"], 1);
}

#[test]
fn the_deadline_defaults_to_the_configured_value() {
    let router = router(Config::default());
    assert_eq!(router.deadline(&json!({})), Duration::from_secs(30));
}

#[test]
fn the_deadline_honours_an_explicit_timeout_argument() {
    let router = router(Config::default());
    assert_eq!(router.deadline(&json!({ "timeout": 7 })), Duration::from_secs(7));
    assert_eq!(router.deadline(&json!({ "timeout_ms": 4500 })), Duration::from_secs(5));
}

#[test]
fn the_deadline_is_clamped_to_the_configured_maximum() {
    let config = Config {
        limits: Limits {
            default_tool_timeout: HumanDuration::secs(5),
            max_tool_timeout: HumanDuration::secs(10),
            ..Default::default()
        },
        ..Default::default()
    };
    let router = router(config);

    assert_eq!(router.deadline(&json!({ "timeout": 9999 })), Duration::from_secs(10));
    assert_eq!(router.deadline(&json!({ "timeout": 0 })), Duration::from_secs(1));
}

#[tokio::test]
async fn tool_policy_flows_from_the_config_into_the_native_tools() {
    let config = Config {
        tools: ToolPolicy { allow_code_execution: true, ..Default::default() },
        ..Default::default()
    };

    let response =
        call(&router(config), "eval_code", json!({ "language": "sh", "code": "echo ok" })).await;
    let result = response.result.unwrap();
    assert_eq!(result["structuredContent"]["stdout"], "ok\n");
}

#[tokio::test]
async fn a_dead_proxy_degrades_to_a_status_entry_instead_of_breaking_tools_list() {
    let config = Config {
        proxies: vec![crate::config::ProxyConfig {
            name: "dead".into(),
            url: "http://127.0.0.1:1/mcp".into(),
            bearer: None,
            headers: std::collections::BTreeMap::new(),
            timeout: HumanDuration::secs(1),
            prefix: None,
            enabled: true,
        }],
        limits: Limits { discovery_timeout: HumanDuration::secs(2), ..Default::default() },
        ..Default::default()
    };
    let router = router(config);

    let tools = router.list_tools().await;
    assert!(tools.iter().any(|t| t.name == "diff_text"), "native tools must still be listed");

    let response = call(&router, "omni_status", json!({})).await;
    let report: Value =
        serde_json::from_str(response.result.unwrap()["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(report["unhealthy_backends"], json!(["dead"]));
}

#[tokio::test]
async fn a_proxy_with_an_unusable_bearer_token_fails_at_build_time() {
    let config = Config {
        proxies: vec![crate::config::ProxyConfig {
            name: "bad".into(),
            url: "http://127.0.0.1:1/mcp".into(),
            bearer: Some("token\nInjected: header".into()),
            headers: std::collections::BTreeMap::new(),
            timeout: HumanDuration::secs(1),
            prefix: None,
            enabled: true,
        }],
        ..Default::default()
    };
    assert!(Router::build(config).is_err());
}

#[tokio::test]
async fn concurrent_calls_all_complete_under_the_concurrency_limit() {
    let config = Config {
        limits: Limits { max_concurrent_calls: 2, ..Default::default() },
        ..Default::default()
    };
    let router = Arc::new(router(config));

    let calls: Vec<_> = (0..10)
        .map(|i| {
            let router = Arc::clone(&router);
            tokio::spawn(async move {
                router
                    .invoke(
                        "count_stats",
                        json!({ "text": "x".repeat(i + 1) }),
                        Duration::from_secs(5),
                    )
                    .await
            })
        })
        .collect();

    for handle in calls {
        assert!(handle.await.unwrap().is_ok());
    }
}

#[tokio::test]
async fn concurrent_callers_share_one_discovery_sweep() {
    // Past the TTL, every arriving caller used to start its own full sweep of
    // every backend — a stampede against the endpoints we are trying not to
    // overload. They must coalesce onto one.
    let config = Config {
        limits: Limits { discovery_ttl: HumanDuration::secs(300), ..Default::default() },
        ..Default::default()
    };
    let router = Arc::new(router(config));
    assert_eq!(router.discovery_count(), 0);

    let callers: Vec<_> = (0..16)
        .map(|_| {
            let router = Arc::clone(&router);
            tokio::spawn(async move { router.list_tools().await.len() })
        })
        .collect();

    for handle in callers {
        assert!(handle.await.unwrap() > 5);
    }
    assert_eq!(router.discovery_count(), 1, "discovery was not coalesced");
}

#[tokio::test]
async fn an_expired_table_is_rediscovered_exactly_once() {
    let config = Config {
        limits: Limits { discovery_ttl: HumanDuration::millis(60), ..Default::default() },
        ..Default::default()
    };
    let router = Arc::new(router(config));

    router.list_tools().await;
    assert_eq!(router.discovery_count(), 1);

    tokio::time::sleep(Duration::from_millis(120)).await;

    let callers: Vec<_> = (0..8)
        .map(|_| {
            let router = Arc::clone(&router);
            tokio::spawn(async move { router.list_tools().await })
        })
        .collect();
    for handle in callers {
        handle.await.unwrap();
    }

    assert_eq!(router.discovery_count(), 2, "the expired table was swept more than once");
}

#[tokio::test]
async fn a_fresh_table_is_not_rediscovered() {
    let router = router(Config::default());
    router.list_tools().await;
    router.list_tools().await;
    router.list_tools().await;
    assert_eq!(router.discovery_count(), 1);
}

#[tokio::test]
async fn omni_status_reports_how_many_sweeps_have_run() {
    let router = router(Config::default());
    let response = call(&router, "omni_status", json!({})).await;
    let report: Value =
        serde_json::from_str(response.result.unwrap()["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(report["discovery_sweeps"], 1);
}
