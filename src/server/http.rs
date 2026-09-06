//! The HTTP transport.
//!
//! This endpoint can run code and rewrite files, so the previous configuration
//! — `CorsLayer::permissive()` and no authentication — meant any page in the
//! user's browser could reach `eval_code` on `localhost:8080`. Now: bearer
//! authentication is required unless explicitly waived, CORS is closed unless
//! origins are listed, and bodies are size-limited.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{get, post};
use axum::{Json, Router as AxumRouter};
use serde_json::json;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

use crate::protocol::{Request, Response, code};
use crate::router::Router;

#[derive(Clone)]
struct AppState {
    router: Arc<Router>,
    /// `None` means authentication was explicitly waived.
    token: Option<Arc<str>>,
}

pub struct HttpConfig {
    pub token: Option<String>,
    pub allowed_origins: Vec<String>,
    pub max_body_bytes: usize,
}

pub fn app(router: Arc<Router>, config: &HttpConfig) -> AxumRouter {
    let state = AppState { router, token: config.token.as_deref().map(Arc::from) };

    AxumRouter::new()
        .route("/mcp", post(handle_rpc))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .layer(cors(&config.allowed_origins))
        .with_state(state)
}

/// Closed by default. Only origins the operator listed are allowed, and
/// credentials are never echoed back to a wildcard.
fn cors(allowed: &[String]) -> CorsLayer {
    if allowed.is_empty() {
        return CorsLayer::new();
    }
    let origins: Vec<HeaderValue> =
        allowed.iter().filter_map(|origin| origin.parse().ok()).collect();
    if origins.len() != allowed.len() {
        warn!("some entries in server.allowed_origins are not valid origins and were ignored");
    }
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([axum::http::Method::POST])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

async fn handle_rpc(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Result<Json<Request>, axum::extract::rejection::JsonRejection>,
) -> HttpResponse {
    if let Err(rejection) = authorize(&state, &headers) {
        return *rejection;
    }

    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            // Preserve the extractor's own status so an oversized body reports
            // 413 rather than being conflated with a syntax error.
            let status = rejection.status();
            return (
                status,
                Json(Response::error(
                    None,
                    code::PARSE_ERROR,
                    format!("could not read the request body: {rejection}"),
                )),
            )
                .into_response();
        }
    };

    match state.router.handle(request).await {
        Some(response) => Json(response).into_response(),
        // A notification is answered with 202 and no body, per the MCP spec.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn authorize(state: &AppState, headers: &axum::http::HeaderMap) -> Result<(), Box<HttpResponse>> {
    let Some(expected) = state.token.as_deref() else { return Ok(()) };

    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .trim();

    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return Ok(());
    }

    warn!("rejected an HTTP request with a missing or incorrect bearer token");
    Err(Box::new(
        (
            StatusCode::UNAUTHORIZED,
            Json(Response::error(None, code::UNAUTHORIZED, "a valid bearer token is required")),
        )
            .into_response(),
    ))
}

/// Compares without leaking the position of the first difference through
/// timing. Lengths are compared first, which only reveals the token length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Arc<Router>,
    config: &HttpConfig,
) -> std::io::Result<()> {
    let address = listener.local_addr()?;
    info!(%address, authenticated = config.token.is_some(), "serving MCP over HTTP");

    axum::serve(listener, app(router, config)).with_graceful_shutdown(shutdown_signal()).await
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn state(token: Option<&str>) -> (Arc<Router>, HttpConfig) {
        let router = Arc::new(Router::build(Config::default()).unwrap());
        let config = HttpConfig {
            token: token.map(str::to_string),
            allowed_origins: Vec::new(),
            max_body_bytes: 1024 * 1024,
        };
        (router, config)
    }

    fn rpc_request(token: Option<&str>, body: &str) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    async fn send(
        token: Option<&str>,
        request: HttpRequest<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let (router, config) = state(token);
        let response = app(router, &config).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(json!(null));
        (status, body)
    }

    #[tokio::test]
    async fn a_correct_token_is_accepted() {
        let (status, body) =
            send(Some("secret"), rpc_request(Some("secret"), r#"{"id":1,"method":"ping"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], 1);
        assert!(body["error"].is_null());
    }

    #[tokio::test]
    async fn a_missing_or_wrong_token_is_rejected() {
        let (status, _) =
            send(Some("secret"), rpc_request(None, r#"{"id":1,"method":"ping"}"#)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) =
            send(Some("secret"), rpc_request(Some("wrong"), r#"{"id":1,"method":"ping"}"#)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], code::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authentication_can_be_waived_explicitly() {
        let (status, _) = send(None, rpc_request(None, r#"{"id":1,"method":"ping"}"#)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_notification_is_accepted_with_no_body() {
        let (router, config) = state(None);
        let response = app(router, &config)
            .oneshot(rpc_request(None, r#"{"method":"notifications/initialized"}"#))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_malformed_body_produces_a_parse_error_not_a_panic() {
        let (status, body) = send(None, rpc_request(None, "{ not json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], code::PARSE_ERROR);
    }

    #[tokio::test]
    async fn the_health_endpoint_needs_no_token() {
        let (router, config) = state(Some("secret"));
        let response = app(router, &config)
            .oneshot(HttpRequest::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_oversized_body_is_rejected() {
        let (router, _) = state(None);
        let config = HttpConfig { token: None, allowed_origins: Vec::new(), max_body_bytes: 32 };
        let response = app(router, &config)
            .oneshot(rpc_request(None, &format!(r#"{{"id":1,"method":"{}"}}"#, "x".repeat(200))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn cors_is_closed_unless_origins_are_configured() {
        let (router, _) = state(None);
        let closed = HttpConfig { token: None, allowed_origins: vec![], max_body_bytes: 1024 };
        let response = app(router, &closed)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"id":1,"method":"ping"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // The old permissive layer echoed the origin back, letting any page
        // read the response.
        assert!(response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[tokio::test]
    async fn a_configured_origin_is_allowed() {
        let (router, _) = state(None);
        let config = HttpConfig {
            token: None,
            allowed_origins: vec!["http://localhost:3000".into()],
            max_body_bytes: 1024,
        };
        let response = app(router, &config)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::ORIGIN, "http://localhost:3000")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"id":1,"method":"ping"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "http://localhost:3000"
        );
    }

    #[test]
    fn constant_time_comparison_is_still_correct() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
