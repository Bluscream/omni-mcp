//! Connection pool for managed SSH sessions.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use super::known_hosts::KnownHostsStore;
use super::session::SshSessionHandle;
use crate::config::SshServerConfig;
use crate::error::ToolResult;

#[derive(Clone)]
pub struct SessionPool {
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<SshSessionHandle>>>>>,
    known_hosts: Arc<KnownHostsStore>,
}

impl SessionPool {
    pub fn new(known_hosts: Arc<KnownHostsStore>) -> Self {
        Self { sessions: Arc::new(Mutex::new(HashMap::new())), known_hosts }
    }

    /// Acquires an existing or fresh connected session for the given server config.
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

        // Connect new session
        let session =
            SshSessionHandle::connect(config, Arc::clone(&self.known_hosts), save_new_fingerprint)
                .await?;

        let arc_session = Arc::new(Mutex::new(session));
        map.insert(config.name.clone(), Arc::clone(&arc_session));
        Ok(arc_session)
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
