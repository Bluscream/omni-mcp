//! Connection pool for managed SSH sessions.
//!
//! Tracks per-server fingerprint mismatch state so that `save_new_fingerprint`
//! is only offered and honoured when a real mismatch has been detected.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use super::known_hosts::KnownHostsStore;
use super::session::SshSessionHandle;
use crate::config::SshServerConfig;
use crate::error::ToolResult;

/// Fingerprint mismatch details captured during a failed connection attempt.
#[derive(Clone, Debug)]
pub struct MismatchInfo {
    pub pinned: String,
    pub received: String,
}

#[derive(Clone)]
pub struct SessionPool {
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<SshSessionHandle>>>>>,
    known_hosts: Arc<KnownHostsStore>,
    /// Pending mismatch state per server name.
    /// Populated when a connection fails due to a fingerprint mismatch.
    /// Cleared when the server successfully connects (with or without re-pinning).
    /// Uses a `std::sync::Mutex` so `descriptors()` can read it without `.await`.
    pub mismatch_pending: Arc<std::sync::Mutex<HashMap<String, MismatchInfo>>>,
    /// Host keys the operator accepted during this process run.
    ///
    /// An override cannot rewrite `fingerprint` in omni-mcp.toml, so without
    /// this every subsequent connection would mismatch again and demand another
    /// override — an approval loop that trains the operator to always say yes.
    accepted: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Mirrors `tools.allow_host_key_learning`.
    allow_learning: bool,
}

impl SessionPool {
    pub fn new(known_hosts: Arc<KnownHostsStore>) -> Self {
        Self::with_learning(known_hosts, true)
    }

    pub fn with_learning(known_hosts: Arc<KnownHostsStore>, allow_learning: bool) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            known_hosts,
            mismatch_pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            accepted: Arc::new(std::sync::Mutex::new(HashMap::new())),
            allow_learning,
        }
    }

    /// Returns true if a fingerprint mismatch is pending for the given server.
    pub fn has_mismatch(&self, server_name: &str) -> bool {
        self.mismatch_pending.lock().is_ok_and(|m| m.contains_key(server_name))
    }

    /// Returns true if any server has a pending fingerprint mismatch.
    pub fn any_mismatch(&self) -> bool {
        self.mismatch_pending.lock().is_ok_and(|m| !m.is_empty())
    }

    /// Acquires an existing or fresh connected session for the given server config.
    ///
    /// `save_new_fingerprint` is only honoured when a mismatch is actually pending for this
    /// server; callers should read `has_mismatch()` before passing `true` here.
    pub async fn get_or_connect(
        &self,
        config: &SshServerConfig,
        save_new_fingerprint: bool,
    ) -> ToolResult<Arc<Mutex<SshSessionHandle>>> {
        let mut map = self.sessions.lock().await;

        if let Some(existing) = map.get(&config.name) {
            let guard = existing.lock().await;
            if guard.is_alive() {
                drop(guard);
                return Ok(Arc::clone(existing));
            }
            info!(server = %config.name, "SSH session expired or closed, reconnecting");
        }

        // Only allow fingerprint re-pinning when a mismatch is actually pending.
        let effective_save = save_new_fingerprint && self.has_mismatch(&config.name);

        // Out-param: populated by connect() on mismatch before returning Err.
        let mismatch_out = std::sync::Mutex::new(None::<(String, String)>);

        let accepted_fingerprint =
            self.accepted.lock().ok().and_then(|a| a.get(&config.name).cloned());

        let result = SshSessionHandle::connect(
            config,
            Arc::clone(&self.known_hosts),
            effective_save,
            self.allow_learning,
            accepted_fingerprint,
            &mismatch_out,
        )
        .await;

        match result {
            Ok(session) => {
                // Successful connect: clear any stale mismatch for this server.
                if let Ok(mut pending) = self.mismatch_pending.lock() {
                    pending.remove(&config.name);
                }
                // Remember a newly accepted key so the next call does not have
                // to be approved all over again.
                if let Some(fingerprint) = session.accepted_fingerprint() {
                    if let Ok(mut accepted) = self.accepted.lock() {
                        accepted.insert(config.name.clone(), fingerprint.to_string());
                    }
                }
                let arc_session = Arc::new(Mutex::new(session));
                map.insert(config.name.clone(), Arc::clone(&arc_session));
                Ok(arc_session)
            }
            Err(e) => {
                // If a mismatch was detected, record it for future calls.
                if let Ok(mut out) = mismatch_out.lock() {
                    if let Some((pinned, received)) = out.take() {
                        if let Ok(mut pending) = self.mismatch_pending.lock() {
                            pending.insert(config.name.clone(), MismatchInfo { pinned, received });
                        }
                    }
                }
                Err(e)
            }
        }
    }

    /// Checks whether a server currently has an active connection.
    pub async fn is_connected(&self, server_name: &str) -> bool {
        let map = self.sessions.lock().await;
        if let Some(existing) = map.get(server_name) {
            existing.lock().await.is_alive()
        } else {
            false
        }
    }
}
