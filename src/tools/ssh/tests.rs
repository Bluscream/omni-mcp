//! Tests for the SSH tool surface.
//!
//! These cover the parts that decide *whether* something is allowed — command
//! filtering, host-key policy, server resolution — rather than the network
//! path, which needs a live server.

use serde_json::json;

use super::*;
use crate::config::{Config, SshServerConfig, ToolPolicy};

fn server(name: &str) -> SshServerConfig {
    SshServerConfig {
        name: name.into(),
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
    }
}

fn ctx(policy: ToolPolicy) -> ToolContext {
    ToolContext::new(policy)
}

fn ssh_enabled() -> ToolPolicy {
    ToolPolicy { allow_ssh: true, ..Default::default() }
}

// ---------------------------------------------------------------- command filtering

#[test]
fn with_no_lists_configured_any_command_is_permitted() {
    assert!(SshTools::validate_command(&server("s"), "rm -rf /tmp/x").is_ok());
}

#[test]
fn a_whitelist_permits_only_matching_commands() {
    let mut cfg = server("s");
    cfg.whitelist = vec!["^systemctl ".into(), "^journalctl ".into()];

    assert!(SshTools::validate_command(&cfg, "systemctl status sshd").is_ok());
    assert!(SshTools::validate_command(&cfg, "journalctl -u sshd").is_ok());

    let err = SshTools::validate_command(&cfg, "cat /etc/shadow").unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
}

#[test]
fn a_blacklist_blocks_matching_commands() {
    let mut cfg = server("s");
    cfg.blacklist = vec!["rm\\s+-rf".into()];

    assert!(SshTools::validate_command(&cfg, "ls -la").is_ok());
    assert!(matches!(SshTools::validate_command(&cfg, "rm -rf /"), Err(ToolError::Denied(_))));
}

#[test]
fn the_blacklist_is_applied_even_to_whitelisted_commands() {
    let mut cfg = server("s");
    cfg.whitelist = vec!["^sudo ".into()];
    cfg.blacklist = vec!["shutdown".into()];

    assert!(SshTools::validate_command(&cfg, "sudo systemctl restart sshd").is_ok());
    assert!(
        SshTools::validate_command(&cfg, "sudo shutdown -h now").is_err(),
        "a blacklist entry must override the whitelist"
    );
}

// ---------------------------------------------------------------- config validation

fn config_with(server: SshServerConfig) -> Result<Config, crate::error::ConfigError> {
    let config = Config { ssh: vec![server], ..Default::default() };
    config.validate()?;
    Ok(config)
}

#[test]
fn an_invalid_blacklist_pattern_is_rejected_at_load() {
    // Previously an unparseable pattern was skipped at match time, so the
    // blacklist failed *open* — a typo silently disabled the guard.
    let mut cfg = server("s");
    cfg.blacklist = vec!["rm(unclosed".into()];

    let err = config_with(cfg).unwrap_err().to_string();
    assert!(err.contains("invalid blacklist pattern"), "{err}");
}

#[test]
fn an_invalid_whitelist_pattern_is_rejected_at_load() {
    let mut cfg = server("s");
    cfg.whitelist = vec!["*bad".into()];
    assert!(config_with(cfg).unwrap_err().to_string().contains("invalid whitelist pattern"));
}

#[test]
fn valid_patterns_are_accepted() {
    let mut cfg = server("s");
    cfg.whitelist = vec!["^ls ".into()];
    cfg.blacklist = vec!["rm\\s+-rf".into()];
    assert!(config_with(cfg).is_ok());
}

#[test]
fn a_server_with_no_authentication_method_is_rejected() {
    let mut cfg = server("s");
    cfg.password = None;
    cfg.private_key = None;

    let err = config_with(cfg).unwrap_err().to_string();
    assert!(err.contains("neither `private_key` nor `password`"), "{err}");
}

#[test]
fn a_malformed_fingerprint_is_rejected_rather_than_read_as_a_mismatch() {
    let mut cfg = server("s");
    cfg.fingerprint = Some("not a fingerprint!".into());

    let err = config_with(cfg).unwrap_err().to_string();
    assert!(err.contains("not a SHA256 digest"), "{err}");
}

// ---------------------------------------------------------------- fingerprints

#[test]
fn fingerprints_normalize_to_the_canonical_prefixed_form() {
    let digest = "n0mNPJd7EjKBBAzYNzB2c9Y0RQFYuPbYmi0mYFakEEo";

    let mut with_prefix = server("s");
    with_prefix.fingerprint = Some(format!("SHA256:{digest}"));

    let mut bare = server("s");
    bare.fingerprint = Some(digest.to_string());

    // A fingerprint pasted without the prefix must not read as a mismatch,
    // which is indistinguishable from an attack.
    assert_eq!(with_prefix.normalized_fingerprint(), bare.normalized_fingerprint());
    assert_eq!(with_prefix.normalized_fingerprint().unwrap(), format!("SHA256:{digest}"));
}

#[test]
fn surrounding_whitespace_in_a_fingerprint_is_tolerated() {
    let mut cfg = server("s");
    cfg.fingerprint = Some("  SHA256:abc  ".into());
    assert_eq!(cfg.normalized_fingerprint().unwrap(), "SHA256:abc");
}

#[test]
fn an_absent_or_blank_fingerprint_is_none() {
    let mut cfg = server("s");
    assert_eq!(cfg.normalized_fingerprint(), None);
    cfg.fingerprint = Some("   ".into());
    assert_eq!(cfg.normalized_fingerprint(), None);
}

// ---------------------------------------------------------------- host-key policy

#[test]
fn host_key_override_is_denied_by_default() {
    // The core protection: a model must not be able to wave away a possible
    // man-in-the-middle by setting a boolean.
    let err = ctx(ssh_enabled()).require_host_key_override().unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)));
    assert!(err.to_string().contains("decision for a human"), "{err}");
}

#[test]
fn host_key_override_is_permitted_once_the_operator_opts_in() {
    let policy = ToolPolicy { allow_host_key_override: true, ..ssh_enabled() };
    assert!(ctx(policy).require_host_key_override().is_ok());
}

#[test]
fn host_key_learning_is_on_by_default_and_can_be_disabled() {
    assert!(ctx(ToolPolicy::default()).allows_host_key_learning());
    let strict = ToolPolicy { allow_host_key_learning: false, ..Default::default() };
    assert!(!ctx(strict).allows_host_key_learning());
}

#[tokio::test]
async fn requesting_an_override_without_a_pending_mismatch_is_a_no_op() {
    let tools = SshTools::with_policy(vec![server("box")], true);
    let granted = tools
        .requested_key_override(
            &json!({ "save_new_fingerprint": true }),
            Some("box"),
            &ctx(ssh_enabled()),
        )
        .unwrap();
    assert!(!granted, "there is nothing to override");
}

#[tokio::test]
async fn not_asking_for_an_override_never_grants_one() {
    let tools = SshTools::with_policy(vec![server("box")], true);
    assert!(!tools.requested_key_override(&json!({}), Some("box"), &ctx(ssh_enabled())).unwrap());
}

#[test]
fn the_override_parameter_is_not_advertised_when_policy_forbids_it() {
    // Offering a parameter that will always be refused just invites the model
    // to keep retrying with it.
    let forbidden = SshTools::with_policy(vec![server("box")], false);
    let schema = &forbidden.descriptors()[0].input_schema;
    assert!(schema["properties"].get("save_new_fingerprint").is_none());
}

// ---------------------------------------------------------------- server resolution

#[test]
fn the_first_enabled_server_becomes_the_default() {
    let tools = SshTools::with_policy(vec![server("first"), server("second")], false);
    assert_eq!(tools.resolve_server(None).unwrap().name, "first");
    assert_eq!(tools.resolve_server(Some("second")).unwrap().name, "second");
}

#[test]
fn disabled_servers_are_not_registered() {
    let mut off = server("off");
    off.enabled = false;
    let tools = SshTools::with_policy(vec![off, server("on")], false);

    assert_eq!(tools.resolve_server(None).unwrap().name, "on");
    assert!(matches!(tools.resolve_server(Some("off")), Err(ToolError::NotFound(_))));
}

#[test]
fn an_unknown_server_name_is_reported() {
    let tools = SshTools::with_policy(vec![server("box")], false);
    assert!(matches!(tools.resolve_server(Some("nope")), Err(ToolError::NotFound(_))));
}

#[test]
fn with_no_servers_configured_resolution_explains_why() {
    let tools = SshTools::with_policy(Vec::new(), false);
    let err = tools.resolve_server(None).unwrap_err();
    assert!(err.to_string().contains("no SSH servers configured"), "{err}");
}

// ---------------------------------------------------------------- tool surface

#[tokio::test]
async fn every_ssh_tool_is_denied_unless_allow_ssh_is_set() {
    let tools = SshTools::with_policy(vec![server("box")], false);
    let denied = ctx(ToolPolicy::default());

    for name in ["ssh_execute", "ssh_transfer", "ssh_list_servers"] {
        let err = tools.call(name, json!({}), &denied).await.unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{name} was not gated: {err:?}");
        assert!(err.to_string().contains("allow_ssh"), "{name}: {err}");
    }
}

#[tokio::test]
async fn an_unknown_tool_name_is_not_found() {
    let tools = SshTools::with_policy(vec![server("box")], false);
    let err = tools.call("ssh_nope", json!({}), &ctx(ssh_enabled())).await.unwrap_err();
    assert!(matches!(err, ToolError::NotFound(_)));
}

#[test]
fn the_advertised_tools_are_well_formed() {
    let tools = SshTools::with_policy(vec![server("box")], false);
    let names: Vec<String> = tools.descriptors().iter().map(|t| t.name.clone()).collect();

    assert!(names.contains(&"ssh_execute".to_string()));
    assert!(names.contains(&"ssh_transfer".to_string()));
    assert!(names.contains(&"ssh_list_servers".to_string()));

    for tool in tools.descriptors() {
        assert_eq!(tool.input_schema["type"], "object", "{} schema", tool.name);
        assert!(
            tool.description.as_deref().is_some_and(|d| d.len() > 20),
            "{} needs a usable description",
            tool.name
        );
    }
}

#[tokio::test]
async fn listing_servers_reports_configuration_without_connecting() {
    // Must not dial anything: listing is a diagnostic.
    let tools = SshTools::with_policy(vec![server("box")], false);

    let started = std::time::Instant::now();
    let result = tools.call("ssh_list_servers", json!({}), &ctx(ssh_enabled())).await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "listing tried to connect");

    let listed = result.structured_content.unwrap();
    assert_eq!(listed[0]["name"], "box");
    assert_eq!(listed[0]["connected"], json!(false));
    assert_eq!(listed[0]["fingerprint_mismatch_pending"], json!(false));
}

#[test]
fn known_hosts_defaults_to_the_standard_openssh_location() {
    let path = known_hosts::KnownHostsStore::default_path();
    assert!(path.ends_with(".ssh/known_hosts"), "{}", path.display());
}
