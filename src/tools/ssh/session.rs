//! SSH session management: connect, authenticate, exec commands, and SFTP transfers.

use russh::ChannelMsg;
use russh::client::{self, Handle, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::{HashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info};

use super::known_hosts::KnownHostsStore;

/// Cap on captured stdout/stderr per remote command, mirroring `eval_code`.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

use crate::config::SshServerConfig;
use crate::error::{ToolError, ToolResult};

/// Key verification result during handshake.
#[derive(Debug, Clone)]
pub enum KeyVerification {
    Trusted,
    NewPinned(String),
    Mismatch { pinned: String, received: String },
}

pub struct ClientKeyHandler {
    host: String,
    port: u16,
    pinned_fingerprint: Option<String>,
    /// A fingerprint the operator accepted earlier in this process run. It
    /// cannot be written back to omni-mcp.toml, so without remembering it every
    /// later connection would mismatch again and demand another override.
    accepted_fingerprint: Option<String>,
    known_hosts: Arc<KnownHostsStore>,
    save_new_fingerprint: bool,
    allow_learning: bool,
    verification: Arc<Mutex<Option<KeyVerification>>>,
}

impl Handler for ClientKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let pk = server_public_key.public_key();
        let received = pk.fingerprint(HashAlg::Sha256).to_string();

        // A fingerprint accepted earlier in this run counts as trusted, so a
        // single override does not have to be repeated for every later call.
        if self.accepted_fingerprint.as_deref() == Some(received.as_str()) {
            let mut v = self.verification.lock().await;
            *v = Some(KeyVerification::Trusted);
            return Ok(true);
        }

        // 1. If fingerprint is configured in omni-mcp.toml, treat it as authoritative
        if let Some(pinned) = &self.pinned_fingerprint {
            if pinned == &received {
                let mut v = self.verification.lock().await;
                *v = Some(KeyVerification::Trusted);
                return Ok(true);
            }
            if self.save_new_fingerprint {
                info!(host = %self.host, port = %self.port, %received, "accepting new host key fingerprint");
                let _ = self.known_hosts.learn(&self.host, self.port, &pk).await;
                let mut v = self.verification.lock().await;
                *v = Some(KeyVerification::NewPinned(received));
                return Ok(true);
            }
            let mut v = self.verification.lock().await;
            *v = Some(KeyVerification::Mismatch { pinned: pinned.clone(), received });
            return Ok(false);
        }

        // 2. Check global ~/.ssh/known_hosts
        match self.known_hosts.check(&self.host, self.port, &pk)? {
            Some(true) => {
                let mut v = self.verification.lock().await;
                *v = Some(KeyVerification::Trusted);
                Ok(true)
            }
            Some(false) => {
                // Key mismatch in ~/.ssh/known_hosts
                if self.save_new_fingerprint {
                    info!(host = %self.host, port = %self.port, %received, "updating host key in known_hosts");
                    let _ = self.known_hosts.learn(&self.host, self.port, &pk).await;
                    let mut v = self.verification.lock().await;
                    *v = Some(KeyVerification::NewPinned(received));
                    Ok(true)
                } else {
                    let mut v = self.verification.lock().await;
                    *v = Some(KeyVerification::Mismatch {
                        pinned: "recorded in ~/.ssh/known_hosts".to_string(),
                        received,
                    });
                    Ok(false)
                }
            }
            None => {
                if !self.allow_learning {
                    let mut v = self.verification.lock().await;
                    *v = Some(KeyVerification::Mismatch {
                        pinned: "no recorded key (host-key learning is disabled)".to_string(),
                        received,
                    });
                    return Ok(false);
                }
                // Host not recorded yet: TOFU learn into ~/.ssh/known_hosts
                info!(host = %self.host, port = %self.port, %received, "recording new host in ~/.ssh/known_hosts");
                let _ = self.known_hosts.learn(&self.host, self.port, &pk).await;
                let mut v = self.verification.lock().await;
                *v = Some(KeyVerification::NewPinned(received));
                Ok(true)
            }
        }
    }
}

pub struct SshSessionHandle {
    handle: Handle<ClientKeyHandler>,
    sftp: Option<SftpSession>,
    /// Set when this connection accepted a previously unknown or changed key.
    accepted_fingerprint: Option<String>,
}

impl SshSessionHandle {
    /// The host key accepted during this handshake, if it was new.
    pub fn accepted_fingerprint(&self) -> Option<&str> {
        self.accepted_fingerprint.as_deref()
    }
}

impl SshSessionHandle {
    /// Connects to an SSH server.
    ///
    /// On fingerprint mismatch `mismatch_out` is populated with `(pinned, received)` fingerprint
    /// strings before returning `Err`. The pool uses this to record pending mismatch state so that
    /// `save_new_fingerprint` can be offered and honoured on the next call.
    pub async fn connect(
        config: &SshServerConfig,
        known_hosts: Arc<KnownHostsStore>,
        save_new_fingerprint: bool,
        allow_learning: bool,
        accepted_fingerprint: Option<String>,
        mismatch_out: &std::sync::Mutex<Option<(String, String)>>,
    ) -> ToolResult<Self> {
        let verification = Arc::new(Mutex::new(None));

        let handler = ClientKeyHandler {
            host: config.host.clone(),
            port: config.port,
            pinned_fingerprint: config.normalized_fingerprint(),
            accepted_fingerprint,
            known_hosts: Arc::clone(&known_hosts),
            save_new_fingerprint,
            allow_learning,
            verification: Arc::clone(&verification),
        };

        let client_config = Arc::new(client::Config::default());

        let addr = format!("{}:{}", config.host, config.port);
        let connect_future = client::connect(client_config, &addr, handler);

        let mut handle = match tokio::time::timeout(Duration::from_secs(15), connect_future).await {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                let v = verification.lock().await.clone();
                if let Some(KeyVerification::Mismatch { pinned, received }) = v {
                    // Record the mismatch so the pool can expose save_new_fingerprint.
                    if let Ok(mut out) = mismatch_out.lock() {
                        *out = Some((pinned.clone(), received.clone()));
                    }
                    return Err(ToolError::Failed(format!(
                        "SECURITY WARNING: Host key fingerprint mismatch for server '{}' ({})! \
                         Expected '{}', but server presented '{}'. \
                         Possible host key rotation or Man-in-the-Middle hijacking! \
                         Re-call with 'save_new_fingerprint: true' to acknowledge and re-pin the new key.",
                        config.name, addr, pinned, received
                    )));
                }
                return Err(ToolError::Failed(format!(
                    "SSH connection to [{}] ({addr}) failed: {e}",
                    config.name
                )));
            }
            Err(_) => {
                return Err(ToolError::Failed(format!(
                    "SSH connection to [{}] ({addr}) timed out after 15s",
                    config.name
                )));
            }
        };

        authenticate(&mut handle, config).await?;

        let accepted = match verification.lock().await.clone() {
            Some(KeyVerification::NewPinned(fp)) => Some(fp),
            _ => None,
        };

        Ok(Self { handle, sftp: None, accepted_fingerprint: accepted })
    }

    pub fn is_alive(&self) -> bool {
        !self.handle.is_closed()
    }

    pub async fn exec(&mut self, cmd: &str) -> ToolResult<(String, String, u32)> {
        let mut channel =
            self.handle.channel_open_session().await.map_err(|e| {
                ToolError::Failed(format!("failed to open SSH session channel: {e}"))
            })?;

        channel
            .exec(true, cmd)
            .await
            .map_err(|e| ToolError::Failed(format!("failed to exec command {cmd:?}: {e}")))?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut truncated = false;
        let mut exit_code = 0;

        // A remote command is untrusted output of unbounded length: `cat
        // /dev/urandom` would otherwise grow these buffers until the process
        // dies. Keep reading so the channel closes cleanly, but stop storing.
        let append = |buffer: &mut Vec<u8>, data: &[u8], truncated: &mut bool| {
            let room = MAX_OUTPUT_BYTES.saturating_sub(buffer.len());
            if room == 0 {
                *truncated = true;
                return;
            }
            let take = data.len().min(room);
            buffer.extend_from_slice(&data[..take]);
            *truncated |= take < data.len();
        };

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => append(&mut stdout, &data, &mut truncated),
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    append(&mut stderr, &data, &mut truncated);
                }
                ChannelMsg::ExitStatus { exit_status } => exit_code = exit_status,
                ChannelMsg::Close => break,
                _ => {}
            }
        }

        let mut out = String::from_utf8_lossy(&stdout).into_owned();
        if truncated {
            use std::fmt::Write as _;
            let _ = write!(out, "\n[omni-mcp: output truncated at {MAX_OUTPUT_BYTES} bytes]");
        }

        Ok((out, String::from_utf8_lossy(&stderr).into_owned(), exit_code))
    }

    pub async fn get_sftp(&mut self) -> ToolResult<&SftpSession> {
        if self.sftp.is_none() {
            let channel =
                self.handle.channel_open_session().await.map_err(|e| {
                    ToolError::Failed(format!("failed to open channel for SFTP: {e}"))
                })?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| ToolError::Failed(format!("failed to request sftp subsystem: {e}")))?;
            let stream = channel.into_stream();
            let session = SftpSession::new(stream).await.map_err(|e| {
                ToolError::Failed(format!("failed to initialize SFTP protocol: {e}"))
            })?;
            self.sftp = Some(session);
        }
        self.sftp.as_ref().ok_or_else(|| ToolError::Failed("SFTP session unavailable".into()))
    }
}

async fn authenticate(
    handle: &mut Handle<ClientKeyHandler>,
    config: &SshServerConfig,
) -> ToolResult<()> {
    let mut auth_ok = false;
    if let Some(key_path) = &config.private_key {
        let res = if let Some(passphrase) = &config.passphrase {
            russh::keys::load_secret_key(key_path, Some(passphrase))
        } else {
            russh::keys::load_secret_key(key_path, None)
        };
        match res {
            Ok(key) => {
                let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                match handle.authenticate_publickey(&config.user, key_with_alg).await {
                    Ok(res) if res.success() => auth_ok = true,
                    Ok(_) => {
                        debug!("public key authentication rejected for {}", config.user);
                    }
                    Err(e) => {
                        return Err(ToolError::Failed(format!(
                            "public key authentication error on [{}]: {e}",
                            config.name
                        )));
                    }
                }
            }
            Err(e) => {
                return Err(ToolError::Failed(format!(
                    "could not load private key '{}' for [{}]: {e}",
                    key_path.display(),
                    config.name
                )));
            }
        }
    }

    if !auth_ok {
        if let Some(password) = &config.password {
            match handle.authenticate_password(&config.user, password).await {
                Ok(res) if res.success() => auth_ok = true,
                Ok(_) => {
                    return Err(ToolError::Denied(format!(
                        "password authentication failed for user '{}' on [{}]",
                        config.user, config.name
                    )));
                }
                Err(e) => {
                    return Err(ToolError::Failed(format!(
                        "authentication error on [{}]: {e}",
                        config.name
                    )));
                }
            }
        }
    }

    if !auth_ok {
        return Err(ToolError::Denied(format!(
            "no valid authentication succeeded for [{}] ({})",
            config.name, config.user
        )));
    }

    Ok(())
}
