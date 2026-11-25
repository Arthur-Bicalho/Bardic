//! OAuth 2.0 Authorization Code + PKCE for Spotify, and token lifecycle.
//! OWNER: auth agent. Public API below is the contract; internals are free.
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::Result;

pub const DEFAULT_ACCOUNTS_BASE: &str = "https://accounts.spotify.com";
/// Scopes Bardic needs for playback control + search.
pub const DEFAULT_SCOPES: &[&str] = &[
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "playlist-read-private",
];
/// Refresh this long before expiry.
pub const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// Public Client ID from the Spotify dashboard (no secret: PKCE).
    pub client_id: String,
    /// `http://127.0.0.1:<port>/callback` (desktop) or `bardic://callback` (Android).
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    /// Overridable for tests (wiremock).
    pub accounts_base_url: String,
}

impl AuthConfig {
    pub fn new(client_id: impl Into<String>, redirect_uri: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
            accounts_base_url: DEFAULT_ACCOUNTS_BASE.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Wall-clock ms (see `Clock`) at which `access_token` expires.
    pub expires_at_ms: u64,
    pub scope: String,
}

/// Persistent, ideally encrypted, storage for tokens. Android implements this in Kotlin
/// (Keystore-backed) via UniFFI; Linux uses `KeyringTokenStore` (feature `keyring-store`).
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<Tokens>>;
    fn save(&self, tokens: &Tokens) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

/// What the engine needs: a currently valid bearer token.
#[async_trait]
pub trait AccessTokenProvider: Send + Sync {
    async fn access_token(&self) -> Result<String>;
}

/// In-memory store (tests, ephemeral sessions).
#[derive(Debug, Default)]
pub struct MemoryTokenStore(std::sync::Mutex<Option<Tokens>>);

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn save(&self, tokens: &Tokens) -> Result<()> {
        *self.0.lock().unwrap() = Some(tokens.clone());
        Ok(())
    }
    fn clear(&self) -> Result<()> {
        *self.0.lock().unwrap() = None;
        Ok(())
    }
}

/// Owns the PKCE login flow and refreshes tokens automatically.
pub struct TokenManager {
    _config: AuthConfig,
    _store: Arc<dyn TokenStore>,
    _clock: Arc<dyn Clock>,
}

impl TokenManager {
    pub fn new(config: AuthConfig, store: Arc<dyn TokenStore>, clock: Arc<dyn Clock>) -> Self {
        Self { _config: config, _store: store, _clock: clock }
    }

    /// Generate verifier/challenge + random `state`, remember them, and return the
    /// `/authorize` URL the UI should open (browser / Custom Tab).
    pub fn begin_login(&self) -> Result<String> {
        todo!("auth agent")
    }

    /// Exchange the code from the redirect. `callback_url` is the full redirect URL
    /// (`...?code=..&state=..` or `?error=..`); state must match the pending login.
    pub async fn complete_login(&self, _callback_url: &str) -> Result<()> {
        todo!("auth agent")
    }

    /// True if refresh/access tokens are stored.
    pub fn is_logged_in(&self) -> bool {
        todo!("auth agent")
    }

    pub fn logout(&self) -> Result<()> {
        todo!("auth agent")
    }
}

#[async_trait]
impl AccessTokenProvider for TokenManager {
    /// Returns the stored access token, refreshing first if it expires within
    /// `REFRESH_MARGIN_MS`. Concurrent callers must share a single refresh request.
    async fn access_token(&self) -> Result<String> {
        todo!("auth agent")
    }
}
