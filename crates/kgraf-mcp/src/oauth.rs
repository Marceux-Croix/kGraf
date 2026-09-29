//! OAuth for hosted Streamable HTTP servers (decision 0051), on `rmcp`'s own
//! `transport::auth` module rather than a vendored copy of codex-rs's
//! `rmcp-client` (see [`crate::client`]'s module docs for why that evaluation came
//! out this way).
//!
//! `rmcp::transport::auth::AuthorizationManager` already does RFC 8414/9728
//! discovery, SEP-991 dynamic client registration, PKCE, token exchange and
//! transparent refresh; it reads and writes tokens through a pluggable
//! [`CredentialStore`] on every call. The only piece this module supplies is that
//! store, backed by the OS keychain through the `keyring` crate, so a token is never
//! written to a plain file.
//!
//! **Scope.** [`OAuthConnector`] drives the manager through the flow
//! (`resolve_metadata` or `resolve_metadata_from_challenge`, `register_client`,
//! `get_authorization_url`, `exchange_code_for_token_with_issuer`) and hands back an
//! [`rmcp::transport::auth::AuthClient`] for
//! [`crate::client::RmcpCaller::connect_streamable_http_authorized`]. Opening a
//! browser at the authorization URL and running a redirect-callback listener is the
//! shell's job, not this crate's: a backend library cannot pop a window.
//! Nothing here is exercised against a hosted authorization server in this crate's
//! tests (the real-speaker rule restricts live tests to local speakers); [`KeyringCredentialStore`]
//! is instead tested against the real OS keychain, which is the actual claim this
//! module makes ("tokens stored via the OS keychain, never in plain files").
//!
//! **Logging.** `rmcp::transport::auth` logs at `debug` include values from an
//! in-flight exchange (an authorization code echoed back from a redirect URL). This
//! crate does not initialize a subscriber (that is the shell's job) and so
//! cannot enforce a filter on another crate's `tracing` output; [`RECOMMENDED_LOG_FILTER`]
//! is the directive whoever does own subscriber setup must add.

pub use crate::oauth_http::{BoundedOAuthHttpClient, MAX_OAUTH_BODY_BYTES, MAX_OAUTH_REDIRECTS};
use kgraf_core::flock::HeldLock;
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, CredentialStore, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The keychain service name every kGraf MCP OAuth token is stored under. The
/// account is the server's own name (`McpServerConfig::name`), so two hosted
/// servers never share an entry.
pub const KEYRING_SERVICE: &str = "kgraf-mcp-oauth";

/// [`KEYRING_SERVICE`] under kGraf's former name, Haustoria (decision 0062).
/// A token stored under it is moved to [`KEYRING_SERVICE`] the first time it is
/// read ([`KeyringCredentialStore::load`]); `clear` removes both.
pub const LEGACY_KEYRING_SERVICE: &str = "haustoria-mcp-oauth";

/// Additive `tracing-subscriber` `EnvFilter` directives. Whoever configures logging
/// must include them (this crate cannot enforce it itself; no kGraf binary
/// installs a subscriber yet):
///
/// - `rmcp::transport::auth=info`: `rmcp`'s own OAuth debug logs can print an
///   authorization code.
/// - `rmcp::transport::streamable_http_client=off`: `rmcp` logs the
///   `Mcp-Session-Id` as a field (`session_id=...`) when it deletes a session, at
///   info, warn and error level. A session id is a bearer of the
///   session, so it stays out of logs. The cost: `rmcp`'s own notes on that
///   target (a failed GET stream, a cleanup timeout) are not logged either; every
///   failure a caller must act on still reaches it as a typed
///   [`crate::McpConnectError`].
pub const RECOMMENDED_LOG_FILTER: &str =
    "rmcp::transport::auth=info,rmcp::transport::streamable_http_client=off";

/// OAuth for one server could not proceed.
#[derive(Debug, thiserror::Error)]
pub enum McpOAuthError {
    #[error("cannot reach the OS keychain for MCP server `{server}`: {reason}")]
    Keychain { server: String, reason: String },

    #[error("MCP server `{server}`'s stored OAuth credentials are corrupt: {reason}")]
    Corrupt { server: String, reason: String },

    #[error("MCP server `{server}`'s configured URL cannot be used for OAuth: {reason}")]
    BadUrl { server: String, reason: String },

    #[error("OAuth with MCP server `{server}` failed: {source}")]
    Auth {
        server: String,
        #[source]
        source: AuthError,
    },
}

/// The keychain payload: credentials plus the origin they were issued for.
///
/// **Why an origin, not just the server's name.** A server's
/// name is a user-chosen label (`McpServerConfig::name`), not a security boundary.
/// If the user later edits the same-named server's configuration to point at a
/// different URL -- a different host entirely, or the same host regained by a
/// different party after a DNS or ownership change -- a token keyed on the name
/// alone would still be handed to whatever now answers at the new URL. Storing the
/// origin the token was issued for, and refusing to return it on a mismatch (rather
/// than erroring, so the caller falls through to a normal re-authorization instead
/// of hard-failing), closes that gap.
#[derive(Serialize, Deserialize)]
struct KeychainEnvelope {
    origin: String,
    credentials: StoredCredentials,
}

/// A [`CredentialStore`] backed by the OS keychain (Keychain on macOS, Credential
/// Manager on Windows, Secret Service on Linux, selected by the `keyring` crate's
/// default features). Blocking keychain calls run on a blocking-pool thread so they
/// never stall the async runtime that drives the OAuth manager.
pub struct KeyringCredentialStore {
    server: String,
    origin: String,
    /// Where the per-server keychain locks live; `None` when this user has no
    /// home directory, which fails every keychain call loudly.
    lock_dir: Option<PathBuf>,
}

impl KeyringCredentialStore {
    /// `origin` is the canonical origin (scheme, host, port) of the server's own
    /// URL, computed by [`canonical_origin`]. A load whose stored origin does not
    /// match this one returns `Ok(None)`, the same as no entry at all.
    pub fn new(server: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            origin: origin.into(),
            lock_dir: default_lock_dir(),
        }
    }

    fn lock_dir(&self) -> Result<PathBuf, AuthError> {
        self.lock_dir.clone().ok_or_else(|| {
            AuthError::CredentialStoreError(
                "no home directory, so no place for the keychain lock; OAuth tokens cannot \
                 be read or written safely"
                    .into(),
            )
        })
    }

    fn entry(&self) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(KEYRING_SERVICE, &self.server)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))
    }

    fn legacy_entry(&self) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(LEGACY_KEYRING_SERVICE, &self.server)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))
    }
}

/// The per-user cache directory the keychain locks live in:
/// `directories::ProjectDirs` for the application "kGraf" (the name the shell's
/// `app_dirs` uses), `locks/` inside it.
fn default_lock_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "kGraf").map(|d| d.cache_dir().join("locks"))
}

/// One server's keychain lock: an exclusive `flock` on
/// `<dir>/mcp-oauth-<server>.lock`, held around every keychain write and the
/// move from the former service, in every kGraf process. A token refresh
/// (`save`) and the move then never interleave: the move's "is the new entry
/// still absent?" check and its copy happen with no refresh in between, so a
/// refreshed token is never overwritten by the older one. Released through
/// [`HeldLock`] (explicit unlock, so a forked child's descriptor copy keeps
/// nothing). A process of the former version takes no such lock; that mixed
/// case keeps only the re-read before the copy.
fn hold_server_lock(dir: &Path, server: &str) -> Result<HeldLock, String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        format!(
            "cannot create the keychain lock directory {}: {e}",
            dir.display()
        )
    })?;
    // The server name is the user's; only letters, digits, `-` and `_` reach
    // the file name. Two names that map to one file only share a lock.
    let safe: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    let path = dir.join(format!("mcp-oauth-{safe}.lock"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open the keychain lock {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("cannot take the keychain lock {}: {e}", path.display()))?;
    Ok(HeldLock::new(file))
}

/// [`load_or_adopt`] under the server's keychain lock.
fn adopt_locked(
    dir: &Path,
    current: &dyn SecretSlot,
    legacy: &dyn SecretSlot,
    server: &str,
) -> Result<(Option<String>, Option<String>), String> {
    let _held = hold_server_lock(dir, server)?;
    load_or_adopt(current, legacy, server)
}

/// A keychain write (a token refresh, a first login) under the server's
/// keychain lock.
fn save_locked(
    dir: &Path,
    current: &dyn SecretSlot,
    server: &str,
    secret: &str,
) -> Result<(), String> {
    let _held = hold_server_lock(dir, server)?;
    current.set(secret)
}

/// One keychain slot, as the move from the former service name needs it, so
/// the move's rule is tested without an OS keychain.
trait SecretSlot {
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, secret: &str) -> Result<(), String>;
    fn delete(&self) -> Result<(), String>;
}

impl SecretSlot for keyring::Entry {
    fn get(&self) -> Result<Option<String>, String> {
        match self.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn set(&self, secret: &str) -> Result<(), String> {
        self.set_password(secret).map_err(|e| e.to_string())
    }
    fn delete(&self) -> Result<(), String> {
        match self.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// `current`'s secret; when it has none and `legacy` (the entry under the
/// former service name) has one, move it first: read the former entry, write
/// the new one, read the new one back, and delete the former one only after
/// that succeeded. A failure before the delete leaves the former entry as it
/// was and is an error (fail loud); a failed delete is said in the notice, since
/// the token is then safely in its new place. The second value is the notice
/// to print, when something moved.
///
/// **Never overwrites a newer token** The
/// keychain has no compare-and-set. Callers run this under the server's
/// keychain lock ([`adopt_locked`]), which every kGraf keychain write also
/// takes ([`save_locked`]), so no kGraf refresh lands between the checks and
/// the copy. The new entry is still read again just before the copy, and a
/// read-back that differs after it wins: those cover a process of the former
/// version (Haustoria), which takes no lock. Only that mixed case keeps a
/// residual window, between the re-read and the copy.
fn load_or_adopt(
    current: &dyn SecretSlot,
    legacy: &dyn SecretSlot,
    server: &str,
) -> Result<(Option<String>, Option<String>), String> {
    if let Some(secret) = current.get()? {
        return Ok((Some(secret), None));
    }
    let Some(secret) = legacy.get().map_err(|e| {
        format!("cannot read the token under the former service {LEGACY_KEYRING_SERVICE}: {e}")
    })?
    else {
        return Ok((None, None));
    };
    let remove_legacy = |mut notice: String| {
        if let Err(e) = legacy.delete() {
            notice.push_str(&format!(
                " The entry under {LEGACY_KEYRING_SERVICE} could not be removed ({e}); remove \
                 it from the keychain by hand."
            ));
        }
        notice
    };
    // Read again right before the copy: a refresh may have written it since.
    if let Some(newer) = current.get()? {
        let notice = remove_legacy(format!(
            "kGraf: MCP server `{server}` already has a token under keychain service \
             {KEYRING_SERVICE}; the older one under {LEGACY_KEYRING_SERVICE} (Haustoria is \
             kGraf's former name, decision 0062) was not copied over it."
        ));
        return Ok((Some(newer), Some(notice)));
    }
    current.set(&secret).map_err(|e| {
        format!(
            "cannot move the token from {LEGACY_KEYRING_SERVICE} to {KEYRING_SERVICE} (the \
             former entry is kept): {e}"
        )
    })?;
    let Some(stored) = current.get()? else {
        return Err(format!(
            "the token written under {KEYRING_SERVICE} did not read back; the entry under \
             {LEGACY_KEYRING_SERVICE} is kept"
        ));
    };
    // `stored` differs from `secret` only when a refresh wrote it after the
    // copy: the refreshed token is the current one.
    let notice = remove_legacy(format!(
        "kGraf: moved the OAuth token of MCP server `{server}` from keychain service \
         {LEGACY_KEYRING_SERVICE} to {KEYRING_SERVICE} (Haustoria is kGraf's former name, \
         decision 0062)."
    ));
    Ok((Some(stored), Some(notice)))
}

/// The canonical origin (`scheme://host[:port]`) of a server's configured URL, for
/// binding a stored OAuth token to the endpoint it was issued for.
pub fn canonical_origin(url: &str) -> Result<String, url::ParseError> {
    Ok(url::Url::parse(url)?.origin().ascii_serialization())
}

#[async_trait::async_trait]
impl CredentialStore for KeyringCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let entry = self.entry()?;
        let legacy = self.legacy_entry()?;
        let server = self.server.clone();
        let dir = self.lock_dir()?;
        let outcome =
            tokio::task::spawn_blocking(move || adopt_locked(&dir, &entry, &legacy, &server))
                .await
                .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        let (json, notice) = outcome.map_err(AuthError::CredentialStoreError)?;
        if let Some(notice) = notice {
            eprintln!("{notice}");
        }
        let Some(json) = json else {
            return Ok(None);
        };
        let envelope: KeychainEnvelope = serde_json::from_str(&json)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        if envelope.origin != self.origin {
            // The server's URL changed since this token was issued: treat it as
            // absent rather than handing a possibly-mismatched credential to
            // whatever now answers at the configured origin.
            return Ok(None);
        }
        Ok(Some(envelope.credentials))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let envelope = KeychainEnvelope {
            origin: self.origin.clone(),
            credentials,
        };
        let json = serde_json::to_string(&envelope)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        let entry = self.entry()?;
        let server = self.server.clone();
        let dir = self.lock_dir()?;
        tokio::task::spawn_blocking(move || save_locked(&dir, &entry, &server, &json))
            .await
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?
            .map_err(AuthError::CredentialStoreError)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        // Both names: a token still under the former one is the same login.
        let entry = self.entry()?;
        let legacy = self.legacy_entry()?;
        let server = self.server.clone();
        let dir = self.lock_dir()?;
        tokio::task::spawn_blocking(move || {
            let _held = hold_server_lock(&dir, &server)?;
            SecretSlot::delete(&entry)?;
            SecretSlot::delete(&legacy)
        })
        .await
        .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?
        .map_err(AuthError::CredentialStoreError)
    }
}

/// Drives one server's OAuth flow. Built fresh per attempt; the manager itself
/// holds no long-lived state beyond what [`KeyringCredentialStore`] persists.
///
/// Takes an attribution sink at construction, the same as [`crate::client::RmcpCaller`]
/// (main-session ruling on decision 0051: OAuth discovery, registration and token
/// exchange are MCP traffic too, and are attributed the same way).
pub struct OAuthConnector {
    server: String,
    manager: AuthorizationManager,
    sink: std::sync::Arc<dyn crate::attribution::McpAttributionSink>,
}

impl OAuthConnector {
    /// Start a connector for `server`, whose base URL is the hosted server's own
    /// Streamable HTTP endpoint (used for RFC 9728 Protected Resource Metadata
    /// discovery, and to bind the stored token to this origin -- see
    /// [`KeyringCredentialStore`]). `proxy` is the server's configured proxy
    /// ([`crate::McpServerConfig::proxy`]); OAuth traffic ignores the
    /// environment's proxy variables, as MCP traffic does.
    pub async fn new(
        server: &str,
        base_url: &str,
        proxy: Option<&str>,
        sink: std::sync::Arc<dyn crate::attribution::McpAttributionSink>,
    ) -> Result<Self, McpOAuthError> {
        let origin = canonical_origin(base_url).map_err(|e| McpOAuthError::BadUrl {
            server: server.to_owned(),
            reason: e.to_string(),
        })?;
        let http = BoundedOAuthHttpClient::new(proxy).map_err(|e| McpOAuthError::BadUrl {
            server: server.to_owned(),
            reason: format!("cannot build the OAuth HTTP client: {}", e.without_url()),
        })?;
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(base_url, std::sync::Arc::new(http))
                .await
                .map_err(|source| McpOAuthError::Auth {
                    server: server.to_owned(),
                    source,
                })?;
        manager.set_credential_store(KeyringCredentialStore::new(server, origin));
        Ok(Self {
            server: server.to_owned(),
            manager,
            sink,
        })
    }

    fn record(&self, started: std::time::Instant, success: bool) {
        use crate::attribution::{McpCallEvent, McpCallKind, McpCallOutcome};
        self.sink.record(McpCallEvent {
            server: self.server.clone(),
            tool: None,
            worker: None,
            task: None,
            kind: McpCallKind::OAuth,
            duration: started.elapsed(),
            outcome: if success {
                McpCallOutcome::Success
            } else {
                McpCallOutcome::ProtocolError
            },
            tool_is_error: None,
            bytes_in: 0,
            bytes_out: 0,
        });
    }

    /// Try to resume from a previously completed authorization, stored in the
    /// keychain. `Ok(true)` means [`Self::into_authorized_client`] can be used
    /// without a new browser round trip (the manager still refreshes the access
    /// token transparently if it has expired).
    pub async fn resume(&mut self) -> Result<bool, McpOAuthError> {
        let started = std::time::Instant::now();
        let result = self.manager.initialize_from_store().await;
        self.record(started, result.is_ok());
        result.map_err(|source| McpOAuthError::Auth {
            server: self.server.clone(),
            source,
        })
    }

    /// Begin a new authorization: discover the server's metadata, register (or
    /// reuse) a client, and return the URL the user must open in a browser.
    pub async fn begin(
        &mut self,
        client_name: &str,
        redirect_uri: &str,
        scopes: &[&str],
    ) -> Result<String, McpOAuthError> {
        let started = std::time::Instant::now();
        let outcome: Result<String, AuthError> = async {
            let resolution = self.manager.resolve_metadata().await?;
            self.manager.set_metadata(resolution.metadata);
            self.manager
                .register_client(client_name, redirect_uri, scopes)
                .await?;
            self.manager.get_authorization_url(scopes).await
        }
        .await;
        self.record(started, outcome.is_ok());
        outcome.map_err(|source| McpOAuthError::Auth {
            server: self.server.clone(),
            source,
        })
    }

    /// Complete the flow with the `code` and `state` (CSRF token) the redirect
    /// callback received. On success the tokens are already persisted to the
    /// keychain by the manager's credential store.
    pub async fn complete(&mut self, code: &str, csrf_token: &str) -> Result<(), McpOAuthError> {
        let started = std::time::Instant::now();
        let result = self
            .manager
            .exchange_code_for_token_with_issuer(code, csrf_token, None)
            .await
            .map(|_| ());
        self.record(started, result.is_ok());
        result.map_err(|source| McpOAuthError::Auth {
            server: self.server.clone(),
            source,
        })
    }

    /// Wrap `http_client` so every request carries this connector's token.
    /// [`crate::client::RmcpCaller::connect_streamable_http_authorized`] calls this
    /// with a client built on [`crate::client::base_http_client_builder`], which
    /// refuses to follow redirects (an unredirected client
    /// cannot be downgraded to `http://` by a malicious redirect response) and
    /// carries the server's configured headers.
    pub fn into_authorized_client<C>(self, http_client: C) -> AuthClient<C> {
        AuthClient::new(http_client, self.manager)
    }
}

/// Whether the real-keychain tests below should run. Gated behind an explicit
/// opt-in: Linux CI has no D-Bus session bus for the
/// Secret Service backend `keyring`'s default features select there, so these
/// tests would fail on every Linux CI run, not just report a false negative. Run
/// them by hand on macOS before a release (`KGRAF_KEYCHAIN_TESTS=1 cargo test
/// -p kgraf-mcp oauth::tests`) as a release gate; they are not part of the
/// default `cargo test --workspace` run.
#[cfg(test)]
fn keychain_tests_enabled() -> bool {
    kgraf_core::legacy::env_var("KGRAF_KEYCHAIN_TESTS").as_deref() == Some("1")
}

/// Deletes a test's keychain entry on drop, including on panic (an assertion failure between `save` and `clear` used to leave a real
/// keychain entry behind on the machine running the test). `Drop` cannot be async,
/// so this reaches the OS keychain directly through a blocking `keyring::Entry`
/// call rather than through the `CredentialStore` trait.
#[cfg(test)]
struct KeychainTestGuard {
    server: String,
}

#[cfg(test)]
impl Drop for KeychainTestGuard {
    fn drop(&mut self) {
        for service in [KEYRING_SERVICE, LEGACY_KEYRING_SERVICE] {
            if let Ok(entry) = keyring::Entry::new(service, &self.server) {
                let _ = entry.delete_credential();
            }
        }
        // The keychain lock file this test's server name made, so a test run
        // leaves nothing in the user's cache directory (the directories are
        // removed only when empty).
        if let Some(dir) = default_lock_dir() {
            let _ = std::fs::remove_file(dir.join(format!("mcp-oauth-{}.lock", self.server)));
            let _ = std::fs::remove_dir(&dir);
            if let Some(parent) = dir.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oauth2::TokenResponse as _;
    use rmcp::transport::auth::OAuthTokenResponse;
    use serde_json::json;
    use std::sync::Mutex;

    /// An in-memory keychain slot whose writes and deletes can be made to fail.
    #[derive(Default)]
    struct Slot {
        secret: Mutex<Option<String>>,
        fail_set: bool,
        fail_delete: bool,
    }

    impl Slot {
        fn holding(secret: &str) -> Self {
            Self {
                secret: Mutex::new(Some(secret.to_owned())),
                ..Self::default()
            }
        }
        fn value(&self) -> Option<String> {
            self.secret.lock().expect("lock").clone()
        }
    }

    impl SecretSlot for Slot {
        fn get(&self) -> Result<Option<String>, String> {
            Ok(self.value())
        }
        fn set(&self, secret: &str) -> Result<(), String> {
            if self.fail_set {
                return Err("denied".into());
            }
            *self.secret.lock().expect("lock") = Some(secret.to_owned());
            Ok(())
        }
        fn delete(&self) -> Result<(), String> {
            if self.fail_delete {
                return Err("denied".into());
            }
            *self.secret.lock().expect("lock") = None;
            Ok(())
        }
    }

    /// A new-service slot into which a token refresh in another process
    /// writes "fresh": before the copy (on the `refresh_on_get`-th read), or
    /// right after it (`refresh_after_set`).
    #[derive(Default)]
    struct Racing {
        secret: Mutex<Option<String>>,
        gets: Mutex<usize>,
        sets: Mutex<usize>,
        refresh_on_get: Option<usize>,
        refresh_after_set: bool,
    }

    impl SecretSlot for Racing {
        fn get(&self) -> Result<Option<String>, String> {
            let mut gets = self.gets.lock().expect("lock");
            *gets += 1;
            if self.refresh_on_get == Some(*gets) {
                *self.secret.lock().expect("lock") = Some("fresh".into());
            }
            Ok(self.secret.lock().expect("lock").clone())
        }
        fn set(&self, secret: &str) -> Result<(), String> {
            *self.sets.lock().expect("lock") += 1;
            let value = if self.refresh_after_set {
                "fresh"
            } else {
                secret
            };
            *self.secret.lock().expect("lock") = Some(value.to_owned());
            Ok(())
        }
        fn delete(&self) -> Result<(), String> {
            *self.secret.lock().expect("lock") = None;
            Ok(())
        }
    }

    /// A refresh that writes the new entry after the first read
    /// is never overwritten by the older token under the former service.
    #[test]
    fn a_token_refreshed_during_the_move_is_never_overwritten() {
        let current = Racing {
            refresh_on_get: Some(2),
            ..Racing::default()
        };
        let legacy = Slot::holding("old");
        let (secret, notice) = load_or_adopt(&current, &legacy, "srv").expect("read");
        assert_eq!(secret.as_deref(), Some("fresh"));
        assert_eq!(*current.sets.lock().expect("lock"), 0, "no copy over it");
        assert_eq!(
            current.secret.lock().expect("lock").as_deref(),
            Some("fresh")
        );
        assert_eq!(legacy.value(), None, "the stale entry is removed");
        assert!(notice.expect("notice").contains("was not copied over it"));

        // A refresh landing right after the copy: its token is the one used.
        let current = Racing {
            refresh_after_set: true,
            ..Racing::default()
        };
        let legacy = Slot::holding("old");
        let (secret, _) = load_or_adopt(&current, &legacy, "srv").expect("read");
        assert_eq!(secret.as_deref(), Some("fresh"));
    }

    /// A new-service slot shared with a concurrent refresh: on its second read
    /// (the re-read just before the copy) it starts a refresh that writes
    /// "fresh" through `save_locked` and gives it time to land, then returns
    /// what it read before.
    struct RefreshOnReread {
        inner: std::sync::Arc<Slot>,
        dir: std::path::PathBuf,
        gets: Mutex<usize>,
        refresh: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl SecretSlot for RefreshOnReread {
        fn get(&self) -> Result<Option<String>, String> {
            let value = self.inner.value();
            let mut gets = self.gets.lock().expect("lock");
            *gets += 1;
            if *gets == 2 {
                let (inner, dir) = (std::sync::Arc::clone(&self.inner), self.dir.clone());
                *self.refresh.lock().expect("lock") = Some(std::thread::spawn(move || {
                    save_locked(&dir, &*inner, "srv", "fresh").expect("refresh");
                }));
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Ok(value)
        }
        fn set(&self, secret: &str) -> Result<(), String> {
            self.inner.set(secret)
        }
        fn delete(&self) -> Result<(), String> {
            self.inner.delete()
        }
    }

    /// A refresh that starts between the re-read and the
    /// copy waits for the server's keychain lock, so it lands after the copy
    /// and the refreshed token is the one kept. Without the lock the copy
    /// would overwrite it with the older token.
    #[test]
    fn a_refresh_during_the_move_waits_for_the_keychain_lock() {
        let dir = tempfile::tempdir().expect("dir");
        let shared = std::sync::Arc::new(Slot::default());
        let current = RefreshOnReread {
            inner: std::sync::Arc::clone(&shared),
            dir: dir.path().to_path_buf(),
            gets: Mutex::new(0),
            refresh: Mutex::new(None),
        };
        let legacy = Slot::holding("old");
        let (secret, _) = adopt_locked(dir.path(), &current, &legacy, "srv").expect("moved");
        assert_eq!(secret.as_deref(), Some("old"), "the move copied first");
        let refresh = current
            .refresh
            .lock()
            .expect("lock")
            .take()
            .expect("started");
        refresh.join().expect("refresh thread");
        assert_eq!(
            shared.value().as_deref(),
            Some("fresh"),
            "the refresh landed last"
        );
        assert_eq!(legacy.value(), None);
    }

    /// Decision 0062: a token under the former service name moves on first
    /// read, and the former entry is removed only after the new one reads back.
    #[test]
    fn a_token_under_the_former_service_moves_on_first_read() {
        let current = Slot::default();
        let legacy = Slot::holding("{\"t\":1}");
        let (secret, notice) = load_or_adopt(&current, &legacy, "srv").expect("moved");
        assert_eq!(secret.as_deref(), Some("{\"t\":1}"));
        assert_eq!(current.value().as_deref(), Some("{\"t\":1}"));
        assert_eq!(legacy.value(), None, "the former entry is removed");
        let notice = notice.expect("a notice");
        assert!(notice.contains(LEGACY_KEYRING_SERVICE), "{notice}");
        assert!(notice.contains(KEYRING_SERVICE), "{notice}");
        // Second read: already moved, no notice.
        let (again, none) = load_or_adopt(&current, &legacy, "srv").expect("read");
        assert_eq!(again.as_deref(), Some("{\"t\":1}"));
        assert_eq!(none, None);
    }

    #[test]
    fn a_failed_write_keeps_the_former_entry_and_fails() {
        let current = Slot {
            fail_set: true,
            ..Slot::default()
        };
        let legacy = Slot::holding("x");
        let err = load_or_adopt(&current, &legacy, "srv").expect_err("fails loud");
        assert!(err.contains("former entry is kept"), "{err}");
        assert_eq!(legacy.value().as_deref(), Some("x"));
    }

    #[test]
    fn a_failed_removal_of_the_former_entry_is_said() {
        let current = Slot::default();
        let legacy = Slot {
            secret: Mutex::new(Some("x".into())),
            fail_delete: true,
            ..Slot::default()
        };
        let (secret, notice) = load_or_adopt(&current, &legacy, "srv").expect("moved");
        assert_eq!(secret.as_deref(), Some("x"));
        assert!(notice.expect("notice").contains("could not be removed"));
    }

    #[test]
    fn the_current_entry_wins_and_nothing_anywhere_is_none() {
        let current = Slot::holding("new");
        let legacy = Slot::holding("old");
        let (secret, notice) = load_or_adopt(&current, &legacy, "srv").expect("read");
        assert_eq!(secret.as_deref(), Some("new"));
        assert_eq!(notice, None);
        assert_eq!(legacy.value().as_deref(), Some("old"), "untouched");
        let (none, _) = load_or_adopt(&Slot::default(), &Slot::default(), "srv").expect("read");
        assert_eq!(none, None);
    }

    /// The move against the real OS keychain (same gate as the tests below).
    #[tokio::test]
    async fn a_token_moves_from_the_former_service_in_the_real_os_keychain() {
        if !keychain_tests_enabled() {
            eprintln!(
                "skipped: set KGRAF_KEYCHAIN_TESTS=1 to run against the real OS \
                 keychain (macOS only -- Linux CI has no D-Bus session bus)"
            );
            return;
        }
        let server = format!("kgraf-mcp-test-legacy-{}", std::process::id());
        let _guard = KeychainTestGuard {
            server: server.clone(),
        };
        let origin = "https://mcp.example.com";
        let envelope = json!({
            "origin": origin,
            "credentials": {
                "client_id": "legacy-client",
                "token_response": null,
                "granted_scopes": [],
            },
        });
        keyring::Entry::new(LEGACY_KEYRING_SERVICE, &server)
            .expect("entry")
            .set_password(&envelope.to_string())
            .expect("seed the former entry");
        let store = KeyringCredentialStore::new(&server, origin);
        let loaded = store.load().await.expect("load").expect("moved token");
        assert_eq!(loaded.client_id, "legacy-client");
        assert!(matches!(
            keyring::Entry::new(LEGACY_KEYRING_SERVICE, &server)
                .expect("entry")
                .get_password(),
            Err(keyring::Error::NoEntry)
        ));
        store.clear().await.expect("clear");
        assert!(store.load().await.expect("load after clear").is_none());
    }

    /// The real mechanism, not the setting (test the mechanism, not the setting): this writes to and reads from
    /// whatever OS keychain backend is available on the machine running the test,
    /// through the same `keyring::Entry` a production run would use. It proves
    /// tokens round-trip through the keychain and are gone after `clear`, not that
    /// a config field merely holds a value.
    #[tokio::test]
    async fn tokens_round_trip_through_the_real_os_keychain() {
        if !keychain_tests_enabled() {
            eprintln!(
                "skipped: set KGRAF_KEYCHAIN_TESTS=1 to run against the real OS \
                 keychain (macOS only -- Linux CI has no D-Bus session bus)"
            );
            return;
        }
        let server = format!("kgraf-mcp-test-{}", std::process::id());
        let _guard = KeychainTestGuard {
            server: server.clone(),
        };
        let store = KeyringCredentialStore::new(&server, "https://mcp.example.com");

        // No entry yet.
        assert!(store.load().await.expect("load").is_none());

        let token: OAuthTokenResponse = serde_json::from_value(json!({
            "access_token": "test-access-token",
            "token_type": "bearer",
            "refresh_token": "test-refresh-token",
        }))
        .expect("a StandardTokenResponse deserializes from the OAuth2 wire shape");
        let stored = StoredCredentials::new(
            "test-client-id".to_owned(),
            Some(token),
            vec!["tools".to_owned()],
            None,
        );
        store.save(stored).await.expect("save");

        let loaded = store
            .load()
            .await
            .expect("load")
            .expect("the entry that was just saved");
        assert_eq!(loaded.client_id, "test-client-id");
        assert_eq!(loaded.granted_scopes, vec!["tools".to_owned()]);
        assert_eq!(
            loaded
                .token_response
                .as_ref()
                .map(|t| t.access_token().secret().clone()),
            Some("test-access-token".to_owned())
        );

        store.clear().await.expect("clear");
        assert!(store.load().await.expect("load after clear").is_none());
    }

    /// A token issued for one origin must not be handed back
    /// once the same-named server's URL has changed to a different origin.
    #[tokio::test]
    async fn a_token_is_not_returned_after_the_servers_origin_changes() {
        if !keychain_tests_enabled() {
            eprintln!(
                "skipped: set KGRAF_KEYCHAIN_TESTS=1 to run against the real OS \
                 keychain (macOS only -- Linux CI has no D-Bus session bus)"
            );
            return;
        }
        let server = format!("kgraf-mcp-test-origin-{}", std::process::id());
        let _guard = KeychainTestGuard {
            server: server.clone(),
        };
        let original = KeyringCredentialStore::new(&server, "https://mcp.example.com");
        let token: OAuthTokenResponse = serde_json::from_value(json!({
            "access_token": "issued-for-example-com",
            "token_type": "bearer",
        }))
        .expect("valid token response");
        original
            .save(StoredCredentials::new(
                "client-id".to_owned(),
                Some(token),
                vec![],
                None,
            ))
            .await
            .expect("save");

        // Same server name, a different origin (as if the user repointed the
        // server's URL): the stored token must not be handed back.
        let moved = KeyringCredentialStore::new(&server, "https://attacker.example.net");
        assert!(
            moved.load().await.expect("load does not error").is_none(),
            "a token issued for a different origin must not be returned"
        );

        // The original origin still sees it, proving the mismatch above was the
        // origin check and not e.g. a missing entry.
        assert!(original.load().await.expect("load").is_some());
    }

    #[tokio::test]
    async fn clearing_an_entry_that_was_never_set_is_not_an_error() {
        if !keychain_tests_enabled() {
            eprintln!(
                "skipped: set KGRAF_KEYCHAIN_TESTS=1 to run against the real OS \
                 keychain (macOS only -- Linux CI has no D-Bus session bus)"
            );
            return;
        }
        let server = format!("kgraf-mcp-test-empty-{}", std::process::id());
        let _guard = KeychainTestGuard {
            server: server.clone(),
        };
        let store = KeyringCredentialStore::new(&server, "https://mcp.example.com");
        store.clear().await.expect("clearing nothing is a no-op");
    }

    #[test]
    fn canonical_origin_ignores_path_and_query() {
        assert_eq!(
            canonical_origin("https://mcp.example.com:8443/mcp?x=1").expect("valid url"),
            "https://mcp.example.com:8443"
        );
        assert_ne!(
            canonical_origin("https://mcp.example.com").expect("a"),
            canonical_origin("https://attacker.example.net").expect("b"),
        );
    }
}
