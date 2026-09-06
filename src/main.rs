use axum::{
    extract::State,
    routing::post,
    Json, Router,
};
use std::env;
use std::fs;
use std::io::{self, BufRead};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod config;
mod modules;
mod registry;
mod traits;
mod types;

use config::Config;
use modules::common::CommonModule;
use modules::diff::DiffModule;
use modules::eval::EvalModule;
use modules::everything::EverythingModule;
use modules::resx::ResxModule;
use registry::Registry;
use types::{JsonRpcRequest, JsonRpcResponse};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    info!("Starting omni-mcp daemon...");

    // Load configuration if present
    let config_path = env::var("OMNI_MCP_CONFIG").unwrap_or_else(|_| "omni-mcp.toml".to_string());
    let config = if let Ok(contents) = fs::read_to_string(&config_path) {
        info!("Loaded configuration from {}", config_path);
        toml::from_str::<Config>(&contents).unwrap_or_default()
    } else {
        info!("No config file found at {}, using default config", config_path);
        Config::default()
    };

    // Initialize Registry & Register Native Modules
    let mut registry = Registry::new(config.clone());
    registry.register(DiffModule::new());
    registry.register(CommonModule::new());
    registry.register(ResxModule::new());
    registry.register(EverythingModule::new());
    registry.register(EvalModule::new());

    let shared_registry = Arc::new(registry);

    // Check if stdio mode requested
    let args: Vec<String> = env::args().collect();
    if args.contains(&"--stdio".to_string()) {
        info!("Running in stdio mode...");
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }

            if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&line) {
                let resp = shared_registry.handle_request(req).await;
                println!("{}", serde_json::to_string(&resp).unwrap_or_default());
            }
        }
        return Ok(());
    }

    // HTTP / SSE Server Mode
    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("Binding HTTP MCP server to http://{}", addr);

    let app = Router::new()
        .route("/mcp", post(handle_mcp))
        .layer(CorsLayer::permissive())
        .with_state(shared_registry);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_mcp(
    State(registry): State<Arc<Registry>>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    let resp = registry.handle_request(req).await;
    Json(resp)
}
