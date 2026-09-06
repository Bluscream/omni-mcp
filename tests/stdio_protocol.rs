// `clippy.toml`'s allow-*-in-tests only covers `#[cfg(test)]` items, not an
// integration-test crate. Panicking on a broken harness is the intent here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! End-to-end tests against the real binary over the stdio transport.
//!
//! These exercise what an IDE actually does: spawn the process, speak
//! newline-delimited JSON-RPC, and expect a clean stdout stream. The previous
//! implementation failed exactly here — logs were written to stdout, corrupting
//! the frame stream, and notifications were answered when they must not be.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn start(config: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni-mcp"));
        command.arg("stdio");
        match config {
            Some(path) => {
                command.arg("--config").arg(path);
            }
            None => {
                // Point at a path that cannot exist so defaults are used and the
                // test never picks up the developer's real configuration.
                command.arg("--config").arg("/nonexistent/omni-mcp.toml");
            }
        }

        let mut child = command
            .env("RUST_LOG", "debug") // prove logs never reach stdout
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the binary should start");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self { child, stdin, stdout }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write");
        self.stdin.flush().expect("flush");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        assert!(!line.trim().is_empty(), "server closed the stream unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|e| {
            panic!("stdout must carry only JSON-RPC frames; got {line:?} ({e})")
        })
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.receive()
    }

    fn handshake(&mut self) {
        let response = self.request(
            1,
            "initialize",
            &json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" }
            }),
        );
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn completes_a_full_session_over_stdio() {
    let mut server = Server::start(None);
    server.handshake();

    // A notification must not produce a frame; the very next frame must be the
    // answer to the request that follows it.
    let tools = server.request(2, "tools/list", &json!({}));
    assert_eq!(tools["id"], 2, "a notification was answered, desynchronising the stream");

    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"omni_status"));
    assert!(names.contains(&"diff_text"));

    let call = server.request(
        3,
        "tools/call",
        &json!({ "name": "count_stats", "arguments": { "text": "one two three" } }),
    );
    assert_eq!(call["id"], 3);
    assert_eq!(call["result"]["structuredContent"]["words"], 3);
}

#[test]
fn responses_are_matched_to_their_request_ids_under_load() {
    let mut server = Server::start(None);
    server.handshake();

    // Pipeline many requests, then read every reply. Ids must all come back.
    for id in 10..30 {
        server.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "count_stats", "arguments": { "text": "x ".repeat(id) } }
        }));
    }

    let mut seen: Vec<u64> = Vec::new();
    for _ in 10..30 {
        let response = server.receive();
        let id = response["id"].as_u64().expect("every reply carries its id");
        assert!(response["error"].is_null(), "{response}");
        assert_eq!(response["result"]["structuredContent"]["words"], id);
        seen.push(id);
    }

    seen.sort_unstable();
    assert_eq!(seen, (10..30).collect::<Vec<u64>>());
}

#[test]
fn stdout_stays_clean_even_with_debug_logging_enabled() {
    // The original build wrote tracing output to stdout, producing frames the
    // client could not parse. `Server::receive` panics on any non-JSON line.
    let mut server = Server::start(None);
    server.handshake();

    for id in 100..105 {
        let response = server.request(id, "ping", &json!({}));
        assert_eq!(response["id"], id);
    }
}

#[test]
fn malformed_input_does_not_kill_the_session() {
    let mut server = Server::start(None);
    server.handshake();

    server.send(&json!({ "jsonrpc": "2.0", "id": 7 })); // no method
    let error = server.receive();
    assert_eq!(error["id"], 7);
    assert!(error["error"].is_object());

    // The connection must still work afterwards.
    let ping = server.request(8, "ping", &json!({}));
    assert_eq!(ping["id"], 8);
    assert!(ping["error"].is_null());
}

#[test]
fn dangerous_tools_are_denied_with_the_default_configuration() {
    let mut server = Server::start(None);
    server.handshake();

    let response = server.request(
        2,
        "tools/call",
        &json!({ "name": "eval_code", "arguments": { "language": "sh", "code": "id" } }),
    );

    assert_eq!(response["result"]["isError"], json!(true));
    let message = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("allow_code_execution"), "{message}");
}

#[test]
fn configured_capabilities_reach_the_tools() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    std::fs::write(workspace.join("notes.txt"), "hello world\n").unwrap();

    let config_path = workspace.join("omni-mcp.toml");
    std::fs::write(
        &config_path,
        format!(
            "[tools]\nallow_file_mutation = true\nallowed_roots = [\"{}\"]\n",
            workspace.display()
        ),
    )
    .unwrap();

    let mut server = Server::start(Some(config_path.to_str().unwrap()));
    server.handshake();

    let response = server.request(
        2,
        "tools/call",
        &json!({
            "name": "grep_search",
            "arguments": {
                "path": workspace.to_str().unwrap(),
                "pattern": "world",
                "replace": "there",
                "apply": true
            }
        }),
    );
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt")).unwrap(), "hello there\n");

    // A path outside the configured roots must still be refused.
    let denied = server.request(
        3,
        "tools/call",
        &json!({ "name": "grep_search", "arguments": { "path": "/etc", "pattern": "root" } }),
    );
    assert_eq!(denied["result"]["isError"], json!(true));
}

#[test]
fn a_broken_configuration_fails_the_check_command() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "[[proxies]]\nname = \"p\"\nurl = \"not-a-url\"\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_omni-mcp"))
        .args(["--config", path.to_str().unwrap(), "check"])
        .output()
        .expect("run check");

    assert!(!output.status.success(), "an invalid config must be a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("http://"), "{stderr}");
}

#[test]
fn the_check_command_accepts_a_valid_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ok.toml");
    std::fs::write(&path, "[server]\nport = 9999\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_omni-mcp"))
        .args(["--config", path.to_str().unwrap(), "check"])
        .output()
        .expect("run check");

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("configuration is valid"));
    assert!(stdout.contains("9999"));
}

#[test]
fn http_mode_refuses_to_start_unauthenticated_without_an_explicit_waiver() {
    let output = Command::new(env!("CARGO_BIN_EXE_omni-mcp"))
        .args(["--config", "/nonexistent/omni-mcp.toml", "serve", "--bind", "127.0.0.1:0"])
        .output()
        .expect("run serve");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--allow-unauthenticated"), "{stderr}");
}

#[test]
fn the_tools_command_lists_tools_as_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_omni-mcp"))
        .args(["--config", "/nonexistent/omni-mcp.toml", "tools", "--json"])
        .output()
        .expect("run tools");

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let tools: Value = serde_json::from_slice(&output.stdout).expect("valid JSON on stdout");
    let names: Vec<&str> =
        tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"hex_view"));
    assert!(names.contains(&"read_resx"));
}

#[test]
fn the_process_exits_when_stdin_closes() {
    let mut server = Server::start(None);
    server.handshake();

    // Closing stdin is how an IDE signals shutdown; the daemon must not linger.
    let stdin = std::mem::replace(&mut server.stdin, {
        let mut placeholder =
            Command::new("true").stdin(Stdio::piped()).spawn().expect("placeholder");
        let handle = placeholder.stdin.take().unwrap();
        let _ = placeholder.wait();
        handle
    });
    drop(stdin);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match server.child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "clean shutdown expected, got {status}");
                return;
            }
            None if std::time::Instant::now() > deadline => {
                panic!("the process did not exit after stdin closed")
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
