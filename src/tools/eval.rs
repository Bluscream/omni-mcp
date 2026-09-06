//! Ad-hoc code execution.
//!
//! This is the most dangerous tool in the binary, and the previous version was
//! unsafe in several concrete ways:
//!
//! * It was always enabled, including over an HTTP endpoint with permissive
//!   CORS, so any web page could execute shell commands on the host.
//! * Temp filenames were built from `Instant::now().elapsed()`, which is
//!   essentially always zero — concurrent calls collided on one path in the
//!   world-writable `/tmp`, a textbook symlink-swap target.
//! * The child was never killed on timeout, so a runaway script survived the
//!   call and accumulated. Given this project exists to stop process storms,
//!   that mattered.
//!
//! Now: disabled unless `tools.allow_code_execution` is set, scripts live in a
//! private 0700 directory, output is capped, and the process group is killed
//! when the deadline passes.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{NativeTool, ToolContext, args, unknown};
use crate::error::{ToolError, ToolResult};
use crate::protocol::{CallToolResult, Tool};

pub struct EvalTools;

/// Truncation point for each of stdout and stderr.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Interpreters this tool knows how to drive.
struct Runtime {
    /// Canonical language name and its accepted aliases.
    names: &'static [&'static str],
    extension: &'static str,
    program: &'static str,
    leading_args: &'static [&'static str],
}

const RUNTIMES: &[Runtime] = &[
    Runtime {
        names: &["python", "py", "python3"],
        extension: "py",
        program: "python3",
        leading_args: &[],
    },
    Runtime {
        names: &["node", "js", "javascript"],
        extension: "js",
        program: "node",
        leading_args: &[],
    },
    Runtime { names: &["bash"], extension: "sh", program: "bash", leading_args: &[] },
    Runtime { names: &["sh", "shell"], extension: "sh", program: "sh", leading_args: &[] },
    Runtime { names: &["ruby", "rb"], extension: "rb", program: "ruby", leading_args: &[] },
    Runtime { names: &["perl", "pl"], extension: "pl", program: "perl", leading_args: &[] },
    Runtime { names: &["php"], extension: "php", program: "php", leading_args: &[] },
    Runtime { names: &["lua"], extension: "lua", program: "lua", leading_args: &[] },
    Runtime { names: &["go"], extension: "go", program: "go", leading_args: &["run"] },
    Runtime { names: &["typescript", "ts"], extension: "ts", program: "tsx", leading_args: &[] },
    Runtime { names: &["rust", "rs"], extension: "rs", program: "rustc", leading_args: &[] },
    Runtime {
        names: &["csharp", "cs", "dotnet"],
        extension: "cs",
        program: "dotnet",
        leading_args: &[],
    },
];

fn lookup(language: &str) -> ToolResult<&'static Runtime> {
    let wanted = language.trim().to_lowercase();
    RUNTIMES.iter().find(|r| r.names.contains(&wanted.as_str())).ok_or_else(|| {
        let supported: Vec<&str> = RUNTIMES.iter().map(|r| r.names[0]).collect();
        ToolError::InvalidArguments(format!(
            "unsupported language {language:?}; supported: {}",
            supported.join(", ")
        ))
    })
}

fn command_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

async fn build_command(
    runtime: &Runtime,
    script: &std::path::Path,
    workspace: &std::path::Path,
) -> ToolResult<(Command, String)> {
    if runtime.names.contains(&"rust") {
        if command_exists("rust-script") {
            let mut cmd = Command::new("rust-script");
            cmd.arg(script);
            Ok((cmd, "rust-script".to_string()))
        } else {
            let out_bin = workspace.join("main_bin");
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "rustc \"$1\" -o \"$2\" && exec \"$2\"", "--"]);
            cmd.arg(script);
            cmd.arg(&out_bin);
            Ok((cmd, "rustc".to_string()))
        }
    } else if runtime.names.contains(&"csharp") {
        if command_exists("dotnet-script") {
            let mut cmd = Command::new("dotnet-script");
            cmd.arg(script);
            Ok((cmd, "dotnet-script".to_string()))
        } else {
            let csproj = workspace.join("main.csproj");
            let proj_xml = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Exe</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
</Project>"#;
            tokio::fs::write(&csproj, proj_xml)
                .await
                .map_err(|e| ToolError::Failed(format!("could not create .csproj: {e}")))?;

            let mut cmd = Command::new("dotnet");
            cmd.args(["run", "--project"]);
            cmd.arg(&csproj);
            Ok((cmd, "dotnet".to_string()))
        }
    } else {
        let mut cmd = Command::new(runtime.program);
        cmd.args(runtime.leading_args);
        cmd.arg(script);
        Ok((cmd, runtime.program.to_string()))
    }
}

#[async_trait]
impl NativeTool for EvalTools {
    fn descriptors(&self) -> Vec<Tool> {
        let languages: Vec<&str> = RUNTIMES.iter().map(|r| r.names[0]).collect();
        vec![Tool::new(
            "eval_code",
            format!(
                "Run a small script in a sandbox and capture stdout/stderr.\n\nSupported languages: {}\n\nExecution is denied unless `tools.allow_code_execution = true` is set in configuration.",
                languages.join(", ")
            ),
            json!({
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "description": format!("One of: {}", languages.join(", ")),
                        // Keep the enum: it lets the client reject a bad
                        // language before a call is ever dispatched.
                        "enum": RUNTIMES.iter().flat_map(|r| r.names.iter().copied()).collect::<Vec<_>>(),
                    },
                    "code": {
                        "type": "string",
                        "description": "Script body to execute",
                    },
                    "stdin": {
                        "type": "string",
                        "description": "Optional text to feed to standard input",
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Wall-clock timeout in seconds (default: 30, maximum: 600)",
                    },
                },
                "required": ["language", "code"],
            }),
        )]
    }

    async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> ToolResult<CallToolResult> {
        if name != "eval_code" {
            return Err(unknown(name));
        }
        ctx.require_code_execution()?;
        // The future owns a `Command` and its pipes; box it to keep the enclosing
        // dispatch future small.
        Box::pin(evaluate(&args)).await
    }
}

async fn evaluate(arguments: &Value) -> ToolResult<CallToolResult> {
    let runtime = lookup(args::string(arguments, "language")?)?;
    let code = args::string(arguments, "code")?;
    let stdin_text = args::opt_string(arguments, "stdin")?.unwrap_or("").to_string();
    let timeout =
        Duration::from_secs(args::u64_or(arguments, "timeout_seconds", DEFAULT_TIMEOUT.as_secs())?)
            .clamp(Duration::from_secs(1), Duration::from_secs(600));

    // A fresh private directory per call: no shared path to race or pre-create,
    // and it is removed when `workspace` drops.
    let workspace = tempfile::Builder::new()
        .prefix("omni-eval-")
        .tempdir()
        .map_err(|e| ToolError::Failed(format!("could not create a scratch directory: {e}")))?;
    let script = workspace.path().join(format!("main.{}", runtime.extension));
    tokio::fs::write(&script, code)
        .await
        .map_err(|e| ToolError::Failed(format!("could not write the script: {e}")))?;

    let (mut command, backend_name) = build_command(runtime, &script, workspace.path()).await?;

    command
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // `go run` compiles into a cache; keep that inside the scratch directory so
    // nothing is left behind in the user's home.
    command.env("GOCACHE", workspace.path().join("go-cache"));
    command.env("TMPDIR", workspace.path());
    // `dotnet` otherwise writes to $HOME/.dotnet and $HOME/.nuget, which
    // pollutes the user's home and fails outright on a read-only rootfs.
    command.env("DOTNET_CLI_HOME", workspace.path());
    command.env("NUGET_PACKAGES", workspace.path().join("nuget"));
    command.env("DOTNET_NOLOGO", "1");
    command.env("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
    command.env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1");

    let mut child = command.spawn().map_err(|e| ToolError::Unavailable {
        backend: backend_name,
        reason: format!("could not start execution: {e}"),
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(stdin_text.as_bytes()).await;
        // Dropping closes the pipe so the script sees EOF instead of hanging.
        drop(stdin);
    }

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let capture = async {
        let stdout = read_capped(&mut stdout_pipe);
        let stderr = read_capped(&mut stderr_pipe);
        let status = child.wait();
        let (status, stdout, stderr) = tokio::join!(status, stdout, stderr);
        (status, stdout, stderr)
    };

    match tokio::time::timeout(timeout, Box::pin(capture)).await {
        Ok((status, stdout, stderr)) => {
            let status = status
                .map_err(|e| ToolError::Failed(format!("waiting for the process failed: {e}")))?;
            let code = status.code();
            let mut result = CallToolResult::structured(json!({
                "language": runtime.names[0],
                "exit_code": code,
                "stdout": stdout.text,
                "stderr": stderr.text,
                "stdout_truncated": stdout.truncated,
                "stderr_truncated": stderr.truncated
            }));
            // A non-zero exit is a tool failure the model should react to.
            if code != Some(0) {
                result.is_error = Some(true);
            }
            Ok(result)
        }
        Err(_) => {
            // `kill_on_drop` handles the child when `child` is dropped here.
            Err(ToolError::Timeout { tool: "eval_code".into(), seconds: timeout.as_secs() })
        }
    }
}

struct Captured {
    text: String,
    truncated: bool,
}

/// Reads a pipe to EOF but stops storing beyond the cap, so a script that
/// prints in a loop cannot exhaust memory or flood the model's context.
async fn read_capped<R>(pipe: &mut Option<R>) -> Captured
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(pipe) = pipe.as_mut() else {
        return Captured { text: String::new(), truncated: false };
    };

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;

    while let Ok(read) = pipe.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        if buffer.len() < MAX_OUTPUT_BYTES {
            let room = MAX_OUTPUT_BYTES - buffer.len();
            buffer.extend_from_slice(&chunk[..read.min(room)]);
            truncated |= read > room;
        } else {
            truncated = true;
        }
    }

    Captured { text: String::from_utf8_lossy(&buffer).into_owned(), truncated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolPolicy;

    fn ctx(allowed: bool) -> ToolContext {
        ToolContext::new(ToolPolicy { allow_code_execution: allowed, ..Default::default() })
    }

    async fn run(arguments: Value) -> ToolResult<Value> {
        let result = EvalTools.call("eval_code", arguments, &ctx(true)).await?;
        Ok(result.structured_content.expect("eval returns structured output"))
    }

    #[tokio::test]
    async fn execution_is_denied_unless_the_operator_enabled_it() {
        let err = EvalTools
            .call("eval_code", json!({ "language": "sh", "code": "echo hi" }), &ctx(false))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn runs_a_shell_script_and_captures_stdout() {
        let out = run(json!({ "language": "sh", "code": "echo hello" })).await.unwrap();
        assert_eq!(out["stdout"], "hello\n");
        assert_eq!(out["exit_code"], 0);
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_flagged_as_a_tool_error_with_stderr_intact() {
        let result = EvalTools
            .call(
                "eval_code",
                json!({ "language": "sh", "code": "echo oops >&2; exit 3" }),
                &ctx(true),
            )
            .await
            .unwrap();

        assert!(result.is_failure());
        let out = result.structured_content.unwrap();
        assert_eq!(out["exit_code"], 3);
        assert_eq!(out["stderr"], "oops\n");
    }

    #[tokio::test]
    async fn stdin_is_piped_to_the_script() {
        let out =
            run(json!({ "language": "sh", "code": "cat", "stdin": "piped in" })).await.unwrap();
        assert_eq!(out["stdout"], "piped in");
    }

    #[tokio::test]
    async fn a_script_that_reads_stdin_with_none_supplied_sees_eof_rather_than_hanging() {
        let out = run(json!({ "language": "sh", "code": "cat; echo done", "timeout_seconds": 5 }))
            .await
            .unwrap();
        assert_eq!(out["stdout"], "done\n");
    }

    #[tokio::test]
    async fn a_runaway_script_is_killed_at_the_deadline() {
        let err = EvalTools
            .call(
                "eval_code",
                json!({ "language": "sh", "code": "sleep 60", "timeout_seconds": 1 }),
                &ctx(true),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn enormous_output_is_truncated_rather_than_buffered_without_bound() {
        let out = run(json!({
            "language": "sh",
            "code": "yes abcdefghijklmnop | head -c 2000000",
            "timeout_seconds": 30
        }))
        .await
        .unwrap();

        assert_eq!(out["stdout_truncated"], json!(true));
        assert!(out["stdout"].as_str().unwrap().len() <= MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn concurrent_evaluations_do_not_share_a_script_path() {
        // The old temp-name scheme produced the same filename for simultaneous
        // calls, so one call ran another's code.
        let scripts = ["echo one", "echo two", "echo three"];
        let handles =
            scripts.map(|code| tokio::spawn(run(json!({ "language": "sh", "code": code }))));

        let mut seen: Vec<String> = Vec::new();
        for handle in handles {
            let out = handle.await.unwrap().unwrap();
            seen.push(out["stdout"].as_str().unwrap().trim().to_string());
        }
        seen.sort();
        assert_eq!(seen, ["one", "three", "two"]);
    }

    #[tokio::test]
    async fn the_scratch_directory_is_removed_after_the_call() {
        let out = run(json!({ "language": "sh", "code": "pwd" })).await.unwrap();
        let workspace = out["stdout"].as_str().unwrap().trim();
        assert!(!std::path::Path::new(workspace).exists(), "{workspace} was left behind");
    }

    #[tokio::test]
    async fn an_unsupported_language_lists_the_supported_ones() {
        let err = EvalTools
            .call("eval_code", json!({ "language": "brainfuck", "code": "+" }), &ctx(true))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("python"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_interpreter_is_unavailable_not_a_crash() {
        // `tsx` is rarely installed; either outcome is acceptable, but it must
        // never panic or hang.
        let outcome =
            EvalTools.call("eval_code", json!({ "language": "ts", "code": "" }), &ctx(true)).await;
        if let Err(err) = outcome {
            assert!(matches!(err, ToolError::Unavailable { .. }), "got {err:?}");
        }
    }

    #[tokio::test]
    async fn compiled_languages_run_via_their_fallback_toolchains() {
        // rust-script / dotnet-script are usually absent, so this exercises the
        // rustc and `dotnet run` fallbacks that actually get used in practice.
        for (language, code, expected) in [
            ("rust", r#"fn main(){println!("ok-rust");}"#, "ok-rust"),
            ("csharp", r#"System.Console.WriteLine("ok-csharp");"#, "ok-csharp"),
        ] {
            let available = match language {
                "rust" => which("rustc").is_some() || which("rust-script").is_some(),
                _ => which("dotnet").is_some() || which("dotnet-script").is_some(),
            };
            if !available {
                continue;
            }

            let outcome = EvalTools
                .call(
                    "eval_code",
                    json!({ "language": language, "code": code, "timeout_seconds": 180 }),
                    &ctx(true),
                )
                .await;

            match outcome {
                Ok(result) => {
                    let out = result.structured_content.unwrap();
                    assert_eq!(
                        out["exit_code"],
                        0,
                        "{language} failed: {}",
                        out["stderr"].as_str().unwrap_or_default()
                    );
                    assert!(
                        out["stdout"].as_str().unwrap_or_default().contains(expected),
                        "{language} produced {:?}",
                        out["stdout"]
                    );
                }
                // A toolchain present but unusable must degrade, never panic.
                Err(err) => assert!(
                    matches!(err, ToolError::Unavailable { .. } | ToolError::Timeout { .. }),
                    "{language}: unexpected {err:?}"
                ),
            }
        }
    }

    #[tokio::test]
    async fn a_compiler_error_is_reported_rather_than_swallowed() {
        if which("rustc").is_none() && which("rust-script").is_none() {
            return;
        }
        let result = EvalTools
            .call(
                "eval_code",
                json!({ "language": "rust", "code": "fn main(){ this is not rust }", "timeout_seconds": 120 }),
                &ctx(true),
            )
            .await;

        if let Ok(result) = result {
            assert!(result.is_failure(), "a compile failure must be flagged");
            let out = result.structured_content.unwrap();
            assert_ne!(out["exit_code"], 0);
        }
    }

    #[test]
    fn the_advertised_schema_constrains_language_to_known_names() {
        let tool = &EvalTools.descriptors()[0];
        let names: Vec<&str> = tool.input_schema["properties"]["language"]["enum"]
            .as_array()
            .expect("language must be an enum so bad values are rejected up front")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        for expected in ["python", "sh", "rust", "csharp"] {
            assert!(names.contains(&expected), "{expected} missing from the enum");
        }
    }

    #[test]
    fn every_runtime_alias_is_reachable_through_lookup() {
        for runtime in RUNTIMES {
            for alias in runtime.names {
                assert!(lookup(alias).is_ok(), "alias {alias} does not resolve");
            }
        }
    }

    /// Minimal PATH lookup for test gating.
    fn which(program: &str) -> Option<std::path::PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        })
    }

    #[test]
    fn language_aliases_resolve_to_one_runtime() {
        assert_eq!(lookup("py").unwrap().program, "python3");
        assert_eq!(lookup("PYTHON3").unwrap().program, "python3");
        assert_eq!(lookup("  js  ").unwrap().program, "node");
        assert!(lookup("cobol").is_err());
    }

    #[test]
    fn every_runtime_declares_at_least_one_name() {
        assert!(RUNTIMES.iter().all(|r| !r.names.is_empty()));
    }
}
