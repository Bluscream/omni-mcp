use async_trait::async_trait;
use serde_json::{json, Value};
use std::env;
use std::process::Stdio;
use tokio::fs;
use tokio::process::Command;
use crate::traits::McpModule;
use crate::types::{CallToolResult, Tool};

pub struct EvalModule;

impl EvalModule {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpModule for EvalModule {
    fn name(&self) -> &'static str {
        "eval"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![Tool {
            name: "eval_code".to_string(),
            description: Some("Evaluates code snippets in python, node, bash, ruby, perl, php, go, lua, or typescript".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "description": "Language: python, node, bash, sh, ruby, perl, php, go, lua, typescript"
                    },
                    "code": { "type": "string", "description": "Code snippet to evaluate" }
                },
                "required": ["language", "code"]
            }),
        }]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        if name != "eval_code" {
            return Err(format!("Unknown tool: {}", name));
        }

        let lang = arguments.get("language").and_then(|v| v.as_str()).ok_or("Missing language")?.to_lowercase();
        let code = arguments.get("code").and_then(|v| v.as_str()).ok_or("Missing code")?;

        let res = match lang.as_str() {
            "python" | "py" => run_script_file(".py", code, "python3").await,
            "node" | "js" | "javascript" => run_script_file(".js", code, "node").await,
            "bash" | "sh" => run_script_file(".sh", code, "bash").await,
            "ruby" | "rb" => run_script_file(".rb", code, "ruby").await,
            "perl" | "pl" => run_script_file(".pl", code, "perl").await,
            "php" => run_script_file(".php", code, "php").await,
            "go" => run_script_file(".go", code, "go").await,
            "lua" => run_script_file(".lua", code, "lua").await,
            "typescript" | "ts" => run_script_file(".ts", code, "tsx").await,
            _ => Err(format!("Unsupported language: {}", lang)),
        }?;

        Ok(CallToolResult::text(res))
    }
}

async fn run_script_file(ext: &str, code: &str, binary: &str) -> Result<String, String> {
    let mut temp_path = env::temp_dir();
    let filename = format!("omni_eval_{}{}", tokio::time::Instant::now().elapsed().as_nanos(), ext);
    temp_path.push(filename);

    fs::write(&temp_path, code).await.map_err(|e| format!("Failed to write temp script: {}", e))?;

    let output_res = match binary {
        "go" => Command::new("go").arg("run").arg(&temp_path).stdout(Stdio::piped()).stderr(Stdio::piped()).output().await,
        _ => Command::new(binary).arg(&temp_path).stdout(Stdio::piped()).stderr(Stdio::piped()).output().await,
    };

    let _ = fs::remove_file(&temp_path).await;

    let output = output_res.map_err(|e| format!("Failed to execute {}: {}", binary, e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push_str("\n--- Stderr ---\n");
        }
        result.push_str(&stderr);
    }

    if result.is_empty() {
        Ok("(no output)".to_string())
    } else {
        Ok(result)
    }
}
