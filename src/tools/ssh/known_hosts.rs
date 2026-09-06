//! SSH host key verification using configured fingerprints or standard OpenSSH `~/.ssh/known_hosts`.

use russh::keys::PublicKey;
use russh::keys::known_hosts;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Debug)]
pub struct KnownHostsStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl KnownHostsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, write_lock: Mutex::new(()) }
    }

    /// Resolves the standard OpenSSH `~/.ssh/known_hosts` path.
    pub fn default_path() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return Path::new(&home).join(".ssh").join("known_hosts");
            }
        }
        PathBuf::from(".ssh/known_hosts")
    }

    /// Checks if a public key matches `known_hosts`.
    /// Returns:
    /// - `Ok(Some(true))` if the key matches the recorded host entry.
    /// - `Ok(Some(false))` if the host is recorded with a DIFFERENT key (mismatch / possible hijack).
    /// - `Ok(None)` if the host is not yet known (TOFU candidate).
    pub fn check(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
    ) -> Result<Option<bool>, russh::Error> {
        if !self.path.exists() {
            return Ok(None);
        }

        // Check if recorded key matches
        if known_hosts::check_known_hosts_path(host, port, key, &self.path)? {
            return Ok(Some(true));
        }

        // Host didn't match directly: see if there are any existing keys for this host
        let existing = known_hosts::known_host_keys_path(host, port, &self.path)?;
        if existing.is_empty() { Ok(None) } else { Ok(Some(false)) }
    }

    /// Appends the server host key to `~/.ssh/known_hosts`.
    pub async fn learn(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), russh::Error> {
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            if let Err(err) = tokio::fs::create_dir_all(parent).await {
                warn!(path = %parent.display(), %err, "could not create .ssh directory");
            }
        }
        info!(host = %host, port = %port, path = %self.path.display(), "saving host key to known_hosts");
        known_hosts::learn_known_hosts_path(host, port, key, &self.path)?;
        Ok(())
    }
}
