//! OAuth 2.0 Authorization Code + PKCE for Spotify, and token lifecycle.
//! OWNER: auth agent. Public API below is the contract; internals are free.
//!
//! Security notes:
//! - Access/refresh tokens, authorization codes and PKCE verifiers are never logged;
//!   `Debug` for [`Tokens`] and [`CallbackParams`] redacts them.
//! - The PKCE verifier and `state` come from the OS CSPRNG (`OsRng`).
//! - Refreshes are single-flight: concurrent [`AccessTokenProvider::access_token`] callers
//!   share one request to the token endpoint.
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::clock::Clock;
use crate::{BardicError, Result};

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

/// Length of the PKCE code verifier (RFC 7636 allows 43..=128).
const VERIFIER_LEN: usize = 64;
/// Length of the OAuth `state` parameter.
const STATE_LEN: usize = 32;
/// RFC 7636 §4.1 "unreserved" characters.
const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
/// URL-safe characters for `state` (no escaping needed anywhere).
const URL_SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Desktop redirect URI for a loopback listener. Spotify rejects `localhost`, so this
/// uses the explicit loopback IP.
pub fn loopback_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// PKCE S256 code challenge: `BASE64URL-NOPAD(SHA256(verifier))` (RFC 7636 §4.2).
#[doc(hidden)]
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_string(charset: &[u8], len: usize) -> String {
    let mut rng = OsRng;
    (0..len)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

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

    fn accounts_url(&self, path: &str) -> String {
        format!("{}{}", self.accounts_base_url.trim_end_matches('/'), path)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Wall-clock ms (see `Clock`) at which `access_token` expires.
    pub expires_at_ms: u64,
    pub scope: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scope", &self.scope)
            .finish()
    }
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

/// OS keychain store (Secret Service on Linux). Tokens are stored as one JSON secret.
#[cfg(feature = "keyring-store")]
#[derive(Debug, Clone)]
pub struct KeyringTokenStore {
    service: String,
    user: String,
}

#[cfg(feature = "keyring-store")]
impl KeyringTokenStore {
    pub const DEFAULT_SERVICE: &'static str = "bardic";
    pub const DEFAULT_USER: &'static str = "spotify";

    /// Service `"bardic"`, user `"spotify"`.
    pub fn new() -> Self {
        Self::with_names(Self::DEFAULT_SERVICE, Self::DEFAULT_USER)
    }

    pub fn with_names(service: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            user: user.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &self.user).map_err(keyring_err)
    }
}

#[cfg(feature = "keyring-store")]
impl Default for KeyringTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "keyring-store")]
fn keyring_err(e: keyring::Error) -> BardicError {
    BardicError::TokenStore(e.to_string())
}

#[cfg(feature = "keyring-store")]
impl TokenStore for KeyringTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        match self.entry()?.get_password() {
            Ok(json) => serde_json::from_str(&json)
                .map(Some)
                // Don't echo the secret contents in the error.
                .map_err(|_| BardicError::TokenStore("stored tokens are not valid JSON".into())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_err(e)),
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let json = serde_json::to_string(tokens)
            .map_err(|e| BardicError::TokenStore(format!("serialize tokens: {e}")))?;
        self.entry()?.set_password(&json).map_err(keyring_err)
    }

    fn clear(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_err(e)),
        }
    }
}

/// `code` + `state` extracted from a successful OAuth redirect.
#[derive(Clone, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
}

impl fmt::Debug for CallbackParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallbackParams")
            .field("code", &"<redacted>")
            .field("state", &"<redacted>")
            .finish()
    }
}

/// Parse an OAuth redirect URL. Accepts an absolute URL (`bardic://callback?...`,
/// `http://127.0.0.1:43821/callback?...`). An `error=` parameter yields
/// `BardicError::Auth`; a missing `state` yields `Auth("state mismatch")`.
pub fn parse_callback(callback_url: &str) -> Result<CallbackParams> {
    let url = Url::parse(callback_url)
        .map_err(|e| BardicError::Auth(format!("invalid callback URL: {e}")))?;
    callback_params(&url)
}

fn callback_params(url: &Url) -> Result<CallbackParams> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => error_description = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(err) = error {
        let msg = match error_description {
            Some(d) => format!("authorization denied: {err} ({d})"),
            None => format!("authorization denied: {err}"),
        };
        return Err(BardicError::Auth(msg));
    }
    let state = state.ok_or_else(state_mismatch)?;
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or_else(|| BardicError::Auth("missing authorization code".into()))?;
    Ok(CallbackParams { code, state })
}

fn state_mismatch() -> BardicError {
    BardicError::Auth("state mismatch".into())
}

struct PendingLogin {
    verifier: String,
    state: String,
}

#[derive(Default)]
struct Cache {
    /// Whether `tokens` reflects the store (lazy load on first use).
    loaded: bool,
    tokens: Option<Tokens>,
}

/// Token endpoint success body.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    scope: Option<String>,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Token endpoint error body (RFC 6749 §5.2).
#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

enum TokenEndpointError {
    InvalidGrant,
    Other(BardicError),
}

impl From<BardicError> for TokenEndpointError {
    fn from(e: BardicError) -> Self {
        TokenEndpointError::Other(e)
    }
}

/// Owns the PKCE login flow and refreshes tokens automatically.
pub struct TokenManager {
    config: AuthConfig,
    store: Arc<dyn TokenStore>,
    clock: Arc<dyn Clock>,
    http: reqwest::Client,
    pending: Mutex<Option<PendingLogin>>,
    cache: Mutex<Cache>,
    /// Serializes refreshes (single-flight).
    refresh_lock: tokio::sync::Mutex<()>,
    /// Bumped on logout/login so an in-flight refresh can't resurrect stale tokens.
    generation: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl TokenManager {
    pub fn new(config: AuthConfig, store: Arc<dyn TokenStore>, clock: Arc<dyn Clock>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            config,
            store,
            clock,
            http,
            pending: Mutex::new(None),
            cache: Mutex::new(Cache::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
        }
    }

    /// Generate verifier/challenge + random `state`, remember them, and return the
    /// `/authorize` URL the UI should open (browser / Custom Tab).
    pub fn begin_login(&self) -> Result<String> {
        let verifier = random_string(UNRESERVED, VERIFIER_LEN);
        let state = random_string(URL_SAFE, STATE_LEN);
        let challenge = pkce_challenge(&verifier);

        let mut url = Url::parse(&self.config.accounts_url("/authorize"))
            .map_err(|e| BardicError::Invalid(format!("accounts base URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &self.config.redirect_uri)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge)
            .append_pair("state", &state)
            .append_pair("scope", &self.config.scopes.join(" "));

        *lock(&self.pending) = Some(PendingLogin { verifier, state });
        tracing::debug!("spotify login started");
        Ok(url.into())
    }

    /// Exchange the code from the redirect. `callback_url` is the full redirect URL
    /// (`...?code=..&state=..` or `?error=..`); state must match the pending login.
    /// A bare path+query (`/callback?code=..`, as seen by a loopback HTTP listener) is
    /// resolved against the configured redirect URI.
    pub async fn complete_login(&self, callback_url: &str) -> Result<()> {
        let url = match Url::parse(callback_url) {
            Ok(u) => u,
            Err(url::ParseError::RelativeUrlWithoutBase) => Url::parse(&self.config.redirect_uri)
                .and_then(|base| base.join(callback_url))
                .map_err(|e| BardicError::Auth(format!("invalid callback URL: {e}")))?,
            Err(e) => return Err(BardicError::Auth(format!("invalid callback URL: {e}"))),
        };
        let params = callback_params(&url)?;

        // Take the pending login only if the state matches, so a forged callback can't
        // cancel the real one. Taking it makes the verifier single-use.
        let verifier = {
            let mut pending = lock(&self.pending);
            match pending.as_ref() {
                None => return Err(BardicError::Auth("no login in progress".into())),
                Some(p) if p.state != params.state => {
                    tracing::warn!("spotify login callback with mismatched state");
                    return Err(state_mismatch());
                }
                Some(_) => pending.take().map(|p| p.verifier).unwrap_or_default(),
            }
        };

        let form = [
            ("grant_type", "authorization_code"),
            ("code", params.code.as_str()),
            ("redirect_uri", self.config.redirect_uri.as_str()),
            ("client_id", self.config.client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ];
        let resp = match self.token_request(&form).await {
            Ok(r) => r,
            Err(TokenEndpointError::InvalidGrant) => {
                return Err(BardicError::Auth(
                    "authorization code rejected (invalid_grant)".into(),
                ))
            }
            Err(TokenEndpointError::Other(e)) => return Err(e),
        };
        let refresh_token = resp
            .refresh_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| BardicError::Auth("token response missing refresh_token".into()))?;
        let tokens = Tokens {
            access_token: resp.access_token,
            refresh_token,
            expires_at_ms: self.expiry(resp.expires_in),
            scope: resp.scope.unwrap_or_default(),
        };

        self.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.cache) = Cache {
            loaded: true,
            tokens: Some(tokens.clone()),
        };
        tracing::info!(
            expires_at_ms = tokens.expires_at_ms,
            "spotify login completed"
        );
        self.store.save(&tokens)
    }

    /// True if refresh/access tokens are stored.
    pub fn is_logged_in(&self) -> bool {
        matches!(self.current_tokens(), Ok(Some(_)))
    }

    pub fn logout(&self) -> Result<()> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.pending) = None;
        *lock(&self.cache) = Cache {
            loaded: true,
            tokens: None,
        };
        tracing::info!("spotify logout");
        self.store.clear()
    }

    fn expiry(&self, expires_in_secs: u64) -> u64 {
        self.clock
            .now_ms()
            .saturating_add(expires_in_secs.saturating_mul(1000))
    }

    /// Cached tokens, loading from the store on first use.
    fn current_tokens(&self) -> Result<Option<Tokens>> {
        let mut cache = lock(&self.cache);
        if !cache.loaded {
            cache.tokens = self.store.load()?;
            cache.loaded = true;
        }
        Ok(cache.tokens.clone())
    }

    fn is_fresh(&self, tokens: &Tokens) -> bool {
        self.clock.now_ms().saturating_add(REFRESH_MARGIN_MS) < tokens.expires_at_ms
    }

    async fn refresh(&self, old: Tokens) -> Result<String> {
        let generation = self.generation.load(Ordering::SeqCst);
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", old.refresh_token.as_str()),
            ("client_id", self.config.client_id.as_str()),
        ];
        tracing::debug!("refreshing spotify access token");
        let resp = match self.token_request(&form).await {
            Ok(r) => r,
            Err(TokenEndpointError::InvalidGrant) => {
                tracing::warn!("spotify refresh token rejected; logging out");
                if self.generation.load(Ordering::SeqCst) == generation {
                    self.logout()?;
                }
                return Err(BardicError::NotAuthenticated);
            }
            Err(TokenEndpointError::Other(e)) => return Err(e),
        };
        let tokens = Tokens {
            access_token: resp.access_token,
            // Spotify may or may not rotate the refresh token.
            refresh_token: resp
                .refresh_token
                .filter(|t| !t.is_empty())
                .unwrap_or(old.refresh_token),
            expires_at_ms: self.expiry(resp.expires_in),
            scope: resp.scope.unwrap_or(old.scope),
        };
        {
            let mut cache = lock(&self.cache);
            // A logout/login happened during the request: don't overwrite it.
            if self.generation.load(Ordering::SeqCst) != generation {
                return match &cache.tokens {
                    Some(t) => Ok(t.access_token.clone()),
                    None => Err(BardicError::NotAuthenticated),
                };
            }
            *cache = Cache {
                loaded: true,
                tokens: Some(tokens.clone()),
            };
        }
        if let Err(e) = self.store.save(&tokens) {
            // Session keeps working from the cache; persistence will be retried on next refresh.
            tracing::warn!(error = %e, "failed to persist refreshed spotify tokens");
        }
        tracing::debug!(
            expires_at_ms = tokens.expires_at_ms,
            "spotify access token refreshed"
        );
        Ok(tokens.access_token)
    }

    async fn token_request(
        &self,
        form: &[(&str, &str)],
    ) -> std::result::Result<TokenResponse, TokenEndpointError> {
        let resp = self
            .http
            .post(self.config.accounts_url("/api/token"))
            .form(form)
            .send()
            .await
            .map_err(|e| BardicError::Network(e.without_url().to_string()))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| BardicError::Network(e.without_url().to_string()))?;
        if status.is_success() {
            return serde_json::from_slice::<TokenResponse>(&body).map_err(|_| {
                TokenEndpointError::Other(BardicError::Auth("malformed token response".into()))
            });
        }
        let err = serde_json::from_slice::<TokenErrorResponse>(&body).ok();
        if status == reqwest::StatusCode::BAD_REQUEST
            && err.as_ref().is_some_and(|e| e.error == "invalid_grant")
        {
            return Err(TokenEndpointError::InvalidGrant);
        }
        let msg = match err {
            Some(TokenErrorResponse {
                error,
                error_description: Some(d),
            }) => {
                format!("token endpoint returned {status}: {error} ({d})")
            }
            Some(TokenErrorResponse {
                error,
                error_description: None,
            }) => {
                format!("token endpoint returned {status}: {error}")
            }
            None => format!("token endpoint returned {status}"),
        };
        Err(TokenEndpointError::Other(BardicError::Auth(msg)))
    }
}

#[async_trait]
impl AccessTokenProvider for TokenManager {
    /// Returns the stored access token, refreshing first if it expires within
    /// `REFRESH_MARGIN_MS`. Concurrent callers must share a single refresh request.
    async fn access_token(&self) -> Result<String> {
        match self.current_tokens()? {
            None => return Err(BardicError::NotAuthenticated),
            Some(t) if self.is_fresh(&t) => return Ok(t.access_token),
            Some(_) => {}
        }
        let _guard = self.refresh_lock.lock().await;
        // Re-check: another caller may have refreshed (or logged out) while we waited.
        match self.current_tokens()? {
            None => Err(BardicError::NotAuthenticated),
            Some(t) if self.is_fresh(&t) => Ok(t.access_token),
            Some(t) => self.refresh(t).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_strings_use_charset_and_length() {
        let v = random_string(UNRESERVED, VERIFIER_LEN);
        assert_eq!(v.len(), 64);
        assert!(v.bytes().all(|b| UNRESERVED.contains(&b)));
        let s = random_string(URL_SAFE, STATE_LEN);
        assert_eq!(s.len(), 32);
        assert!(s.bytes().all(|b| URL_SAFE.contains(&b)));
        assert_ne!(v, random_string(UNRESERVED, VERIFIER_LEN));
    }

    #[test]
    fn loopback_uri_uses_ip() {
        assert_eq!(
            loopback_redirect_uri(43821),
            "http://127.0.0.1:43821/callback"
        );
    }

    #[test]
    fn parse_callback_variants() {
        let p = parse_callback("bardic://callback?code=abc&state=xyz").unwrap();
        assert_eq!((p.code.as_str(), p.state.as_str()), ("abc", "xyz"));
        assert!(matches!(
            parse_callback("bardic://callback?error=access_denied&state=xyz"),
            Err(BardicError::Auth(m)) if m.contains("access_denied")
        ));
        assert!(matches!(
            parse_callback("bardic://callback?code=abc"),
            Err(BardicError::Auth(m)) if m == "state mismatch"
        ));
    }

    #[test]
    fn debug_redacts_secrets() {
        let t = Tokens {
            access_token: "AT-secret".into(),
            refresh_token: "RT-secret".into(),
            expires_at_ms: 1,
            scope: "s".into(),
        };
        let d = format!("{t:?}");
        assert!(!d.contains("AT-secret") && !d.contains("RT-secret"));
    }
}
