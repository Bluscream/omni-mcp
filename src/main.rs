use std::sync::Arc;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use omni_mcp::cli::{Cli, Command};
use omni_mcp::config::Config;
use omni_mcp::error::StartupError;
use omni_mcp::router::Router;
use omni_mcp::server::{HttpConfig, http, stdio};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_logging(cli.log.as_deref());

    match run(&cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // stderr, never stdout: stdout is the JSON-RPC stream.
            eprintln!("omni-mcp: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            std::process::ExitCode::FAILURE
        }
    }
}

/// Logs go to stderr. Writing them to stdout corrupts the stdio transport,
/// which is how the original build produced unparseable frames.
fn init_logging(level: Option<&str>) {
    let filter = level.map_or_else(
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        EnvFilter::new,
    );

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

async fn run(cli: &Cli) -> Result<(), StartupError> {
    let path = cli.config_path();
    let config = Config::load_or_default(&path)?;
    info!(config = %path.display(), "configuration loaded");

    match cli.command() {
        Command::Check => check(&path, &config),
        Command::Tools { json } => list_tools(config, *json).await,
        Command::Stdio => {
            let router = build(config).await?;
            stdio::serve(router).await.map_err(StartupError::Io)
        }
        Command::Serve { bind, allow_unauthenticated } => {
            serve_http(config, bind.as_deref(), *allow_unauthenticated).await
        }
    }
}

async fn build(config: Config) -> Result<Arc<Router>, StartupError> {
    let router = Router::build(config).map_err(|err| {
        StartupError::Config(omni_mcp::error::ConfigError::Invalid(err.to_string()))
    })?;
    let router = Arc::new(router);
    router.warm_up().await;
    Ok(router)
}

async fn serve_http(
    config: Config,
    bind: Option<&str>,
    allow_unauthenticated: bool,
) -> Result<(), StartupError> {
    // Refuse to expose code execution and file writes without authentication
    // unless the operator says so explicitly.
    if config.server.auth_token().is_none() && !allow_unauthenticated {
        return Err(StartupError::UnauthenticatedHttp);
    }

    let address = bind
        .map_or_else(|| format!("{}:{}", config.server.host, config.server.port), str::to_string);
    let http_config = HttpConfig {
        token: config.server.auth_token().map(str::to_string),
        allowed_origins: config.server.allowed_origins.clone(),
        max_body_bytes: config.server.max_body_bytes,
    };

    let router = build(config).await?;
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|source| StartupError::Bind { addr: address.clone(), source })?;

    http::serve(listener, router, &http_config).await.map_err(StartupError::Io)
}

fn check(path: &std::path::Path, config: &Config) -> Result<(), StartupError> {
    config.validate()?;

    println!("configuration: {}", path.display());
    println!("  bind:                 {}:{}", config.server.host, config.server.port);
    println!(
        "  http auth:            {}",
        if config.server.auth_token().is_some() { "bearer token set" } else { "NOT SET" }
    );
    println!(
        "  cors origins:         {}",
        if config.server.allowed_origins.is_empty() {
            "none (closed)".to_string()
        } else {
            config.server.allowed_origins.join(", ")
        }
    );
    println!("  code execution:       {}", enabled(config.tools.allow_code_execution));
    println!("  file mutation:        {}", enabled(config.tools.allow_file_mutation));
    println!(
        "  allowed roots:        {}",
        if config.tools.allowed_roots.is_empty() {
            "unrestricted".to_string()
        } else {
            config
                .tools
                .allowed_roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("  max concurrent calls: {}", config.limits.max_concurrent_calls);
    println!("  max concurrent spawns:{}", config.limits.max_concurrent_spawns);

    println!("  sidecars:");
    for sidecar in &config.sidecars {
        println!(
            "    - {:<20} {} {} ({})",
            sidecar.name,
            sidecar.command,
            sidecar.args.join(" "),
            if sidecar.enabled { if sidecar.lazy { "lazy" } else { "eager" } } else { "disabled" }
        );
    }
    println!("  proxies:");
    for proxy in &config.proxies {
        println!(
            "    - {:<20} {} ({}{})",
            proxy.name,
            proxy.url,
            if proxy.enabled { "enabled" } else { "disabled" },
            if proxy.bearer.is_some() { ", authenticated" } else { "" }
        );
    }

    println!("\nconfiguration is valid.");
    Ok(())
}

fn enabled(flag: bool) -> &'static str {
    if flag { "enabled" } else { "disabled" }
}

async fn list_tools(config: Config, as_json: bool) -> Result<(), StartupError> {
    let router = build(config).await?;
    let tools = router.list_tools().await;

    if as_json {
        let rendered = serde_json::to_string_pretty(&tools)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
        println!("{rendered}");
        return Ok(());
    }

    println!("{} tools available:\n", tools.len());
    for tool in tools {
        let description = tool.description.unwrap_or_default();
        let summary = description.lines().next().unwrap_or_default();
        println!("  {:<24} {summary}", tool.name);
    }
    Ok(())
}
