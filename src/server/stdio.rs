//! The stdio transport used by IDEs and Claude Desktop.
//!
//! The previous implementation read stdin with the blocking `std::io` API from
//! inside the async runtime, so the reactor stalled for the duration of every
//! tool call and requests were strictly serialised. It also wrote responses
//! with `println!`, mixing the JSON-RPC stream with anything else that reached
//! stdout. Here reading, working and writing are separate tasks, and stdout
//! carries nothing but framed responses — logs go to stderr.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::protocol::{Request, Response, code};
use crate::router::Router;

/// Bounds how many parsed-but-unanswered requests can queue up.
const CHANNEL_DEPTH: usize = 64;

pub async fn serve(router: Arc<Router>) -> std::io::Result<()> {
    info!("serving MCP over stdio");

    let (outbound, mut outbox) = mpsc::channel::<Response>(CHANNEL_DEPTH);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(response) = outbox.recv().await {
            match serde_json::to_string(&response) {
                Ok(mut line) => {
                    line.push('\n');
                    if stdout.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    // Flush per message: a client blocks waiting for this reply.
                    if stdout.flush().await.is_err() {
                        break;
                    }
                }
                Err(err) => error!(%err, "could not encode a response"),
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut in_flight = tokio::task::JoinSet::new();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                debug!(%err, "unparseable input");
                // Only answer if we can tell it was meant to be a request.
                if let Some(id) = salvage_id(&line) {
                    let _ = outbound
                        .send(Response::error(
                            Some(id),
                            code::PARSE_ERROR,
                            format!("could not parse the request: {err}"),
                        ))
                        .await;
                }
                continue;
            }
        };

        let router = Arc::clone(&router);
        let outbound = outbound.clone();
        in_flight.spawn(async move {
            if let Some(response) = router.handle(request).await {
                let _ = outbound.send(response).await;
            }
        });

        // Reap finished work so the set does not grow without bound.
        while in_flight.try_join_next().is_some() {}
    }

    info!("stdin closed; draining in-flight requests");
    while in_flight.join_next().await.is_some() {}
    drop(outbound);
    let _ = writer.await;
    Ok(())
}

/// Recovers the `id` from a malformed request so the client is not left
/// waiting forever for a reply that will never come.
fn salvage_id(line: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value.get("id").cloned().filter(|id| !id.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_id_is_recovered_from_a_structurally_valid_but_invalid_request() {
        // Valid JSON, but `method` is missing so it fails to deserialize.
        let id = salvage_id(r#"{"jsonrpc":"2.0","id":42}"#);
        assert_eq!(id, Some(json!(42)));
    }

    #[test]
    fn no_id_is_recovered_from_unparseable_or_notification_input() {
        assert_eq!(salvage_id("not json at all"), None);
        assert_eq!(salvage_id(r#"{"method":"notifications/x"}"#), None);
        assert_eq!(salvage_id(r#"{"id":null,"method":"x"}"#), None);
    }

    #[test]
    fn a_string_id_is_preserved_verbatim() {
        assert_eq!(salvage_id(r#"{"id":"abc"}"#), Some(json!("abc")));
    }
}
