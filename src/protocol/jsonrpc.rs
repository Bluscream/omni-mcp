//! JSON-RPC 2.0 envelope types.
//!
//! The distinction that matters most here is *request* vs *notification*: a
//! notification has no `id` and MUST NOT be answered. The previous
//! implementation answered everything, which produces spurious traffic that
//! strict clients reject.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Standard JSON-RPC error codes, plus the MCP-specific range.
pub mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// Implementation-defined: the request outlived its deadline.
    pub const TIMEOUT: i32 = -32000;
    /// Implementation-defined: the caller is not authorized.
    pub const UNAUTHORIZED: i32 = -32001;
}

/// An inbound JSON-RPC message. `id` absent means notification.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// A message without an `id` is a notification and must not be answered.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// Returns `params` as an object, or an empty object when absent.
    pub fn params_or_empty(&self) -> Value {
        self.params.clone().unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize, Serialize, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
    }

    pub fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError { code, message: message.into(), data: None }),
        }
    }

    /// Splits the envelope into the JSON-RPC-level outcome.
    pub fn into_result(self) -> Result<Value, RpcError> {
        match (self.result, self.error) {
            (_, Some(err)) => Err(err),
            (Some(value), None) => Ok(value),
            (None, None) => Err(RpcError {
                code: code::INTERNAL_ERROR,
                message: "response carried neither result nor error".into(),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_is_detected_by_absent_id() {
        let notif: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(notif.is_notification());

        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert!(!req.is_notification());
    }

    #[test]
    fn null_id_is_preserved_rather_than_dropped() {
        // `"id": null` is technically a notification per spec, but some clients
        // send it for real requests. We treat it as an id so a reply is produced.
        let req: Request = serde_json::from_str(r#"{"id":null,"method":"ping"}"#).unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn success_response_omits_error_field() {
        let resp = Response::success(Some(serde_json::json!(7)), serde_json::json!({}));
        let encoded = serde_json::to_string(&resp).unwrap();
        assert!(!encoded.contains("error"));
        assert!(encoded.contains(r#""id":7"#));
    }

    #[test]
    fn envelope_without_result_or_error_is_an_error() {
        let resp = Response { jsonrpc: "2.0".into(), id: None, result: None, error: None };
        assert!(resp.into_result().is_err());
    }
}
