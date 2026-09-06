use std::time::Duration;

use super::*;

fn parse(toml_text: &str) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(toml_text)
        .map_err(|source| ConfigError::Parse { path: "<test>".into(), source: Box::new(source) })?;
    config.validate()?;
    Ok(config)
}

#[test]
fn empty_config_is_valid_and_fully_defaulted() {
    let config = parse("").unwrap();
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 8080);
    assert_eq!(config.limits.default_tool_timeout.get(), Duration::from_secs(30));
    assert!(config.proxies.is_empty());
    assert!(config.sidecars.is_empty());
}

#[test]
fn dangerous_tools_are_disabled_unless_opted_into() {
    let config = parse("").unwrap();
    assert!(!config.tools.allow_code_execution);
    assert!(!config.tools.allow_file_mutation);
}

#[test]
fn a_blank_auth_token_counts_as_no_authentication() {
    // `auth_token = "${OMNI_MCP_TOKEN:-}"` with the variable unset must not be
    // mistaken for a configured credential.
    for value in ["", "   "] {
        let config = parse(&format!("[server]\nauth_token = \"{value}\"\n")).unwrap();
        assert_eq!(config.server.auth_token(), None);
    }
    let config = parse("[server]\nauth_token = \" real \"\n").unwrap();
    assert_eq!(config.server.auth_token(), Some("real"));
}

#[test]
fn cors_is_closed_and_http_auth_absent_by_default() {
    let config = parse("").unwrap();
    assert!(config.server.allowed_origins.is_empty());
    assert!(config.server.auth_token().is_none());
}

#[test]
fn sidecar_spawns_are_serialised_by_default() {
    // The whole point of the rewrite: no thundering herd of child processes.
    assert_eq!(parse("").unwrap().limits.max_concurrent_spawns, 1);
}

#[test]
fn parses_a_full_configuration() {
    let config = parse(
        r#"
        [server]
        host = "0.0.0.0"
        port = 9000
        auth_token = "abc"
        allowed_origins = ["http://localhost:3000"]

        [limits]
        default_tool_timeout = "10s"
        max_tool_timeout = "1m"
        max_concurrent_calls = 4

        [tools]
        allow_code_execution = true
        allowed_roots = ["/srv/data"]

        [[proxies]]
        name = "ha"
        url = "http://192.168.2.4:8123/api/mcp"
        bearer = "tok"
        prefix = "ha_"

        [[sidecars]]
        name = "browser"
        command = "node"
        args = ["server.js"]
        lazy = false
    "#,
    )
    .unwrap();

    assert_eq!(config.server.port, 9000);
    assert_eq!(config.limits.max_tool_timeout.get(), Duration::from_secs(60));
    assert!(config.tools.allow_code_execution);
    assert_eq!(config.tools.allowed_roots, vec![PathBuf::from("/srv/data")]);
    assert_eq!(config.proxies[0].prefix.as_deref(), Some("ha_"));
    assert!(!config.sidecars[0].lazy);
}

#[test]
fn token_is_accepted_as_an_alias_for_bearer() {
    let config = parse(
        r#"
        [[proxies]]
        name = "p"
        url = "http://example.invalid/mcp"
        token = "legacy"
    "#,
    )
    .unwrap();
    assert_eq!(config.proxies[0].bearer.as_deref(), Some("legacy"));
}

#[test]
fn typos_are_rejected_instead_of_silently_ignored() {
    // The old loader fell back to `Config::default()` on any parse failure, so
    // a misspelled key silently discarded the entire configuration.
    let err = parse("[server]\nprot = 9000\n").unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn duplicate_backend_names_are_rejected() {
    let err = parse(
        r#"
        [[proxies]]
        name = "dup"
        url = "http://a.invalid/mcp"

        [[sidecars]]
        name = "dup"
        command = "true"
    "#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("duplicate backend name"));
}

#[test]
fn non_http_proxy_urls_are_rejected() {
    let err = parse("[[proxies]]\nname = \"p\"\nurl = \"ws://x.invalid\"\n").unwrap_err();
    assert!(err.to_string().contains("expected an http://"));
}

#[test]
fn empty_sidecar_command_is_rejected() {
    let err = parse("[[sidecars]]\nname = \"s\"\ncommand = \"  \"\n").unwrap_err();
    assert!(err.to_string().contains("empty command"));
}

#[test]
fn zero_concurrency_is_rejected() {
    assert!(parse("[limits]\nmax_concurrent_calls = 0\n").is_err());
    assert!(parse("[limits]\nmax_concurrent_spawns = 0\n").is_err());
}

#[test]
fn default_timeout_may_not_exceed_the_hard_cap() {
    let err = parse("[limits]\ndefault_tool_timeout = \"10m\"\nmax_tool_timeout = \"30s\"\n")
        .unwrap_err();
    assert!(err.to_string().contains("cannot exceed"));
}

#[test]
fn disabled_backends_are_filtered_out() {
    let config = parse(
        r#"
        [[proxies]]
        name = "on"
        url = "http://a.invalid/mcp"

        [[proxies]]
        name = "off"
        url = "http://b.invalid/mcp"
        enabled = false
    "#,
    )
    .unwrap();
    let names: Vec<_> = config.enabled_proxies().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["on"]);
}

#[test]
fn secrets_are_read_from_the_environment_at_load_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("omni-mcp.toml");
    std::fs::write(
        &path,
        "[[proxies]]\nname = \"ha\"\nurl = \"${HA_URL:-http://d.invalid/mcp}\"\nbearer = \"${OMNI_TEST_TOKEN}\"\n",
    )
    .unwrap();

    // SAFETY: single-threaded test process section; no other thread reads env here.
    unsafe { std::env::set_var("OMNI_TEST_TOKEN", "from-env") };
    let config = Config::load(&path).unwrap();
    unsafe { std::env::remove_var("OMNI_TEST_TOKEN") };

    assert_eq!(config.proxies[0].bearer.as_deref(), Some("from-env"));
    assert_eq!(config.proxies[0].url, "http://d.invalid/mcp");
}

#[test]
fn missing_config_file_yields_defaults_but_a_bad_one_errors() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.toml");
    assert!(Config::load_or_default(&missing).is_ok());

    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "this is not toml {{{").unwrap();
    assert!(Config::load_or_default(&bad).is_err());
}
