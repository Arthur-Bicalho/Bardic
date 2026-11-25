//! OAuth 2.0 Authorization Code + PKCE for Spotify, and token lifecycle.
//! OWNER: auth agent. Public API below is the contract; internals are free.
//!
//! Security notes:
//! - Access/refresh tokens, authorization codes and PKCE verifiers are never logged;
//!   `Debug` for [`Tokens`] and [`CallbackParams`] redacts them.
//! - The PKCE verifier and `state` come from the OS CSPRNG (`OsRng`).
//! - Refreshes are single-flight: concurrent [`AccessTokenProvider::access_token`] callers
//!   share one request to the token endpoint.
//! - `state` is compared in constant time and checked before acting on any callback
//!   (including `error=` ones); pending logins expire after [`PENDING_LOGIN_TTL_MS`].
//! - Login, logout and refresh results are persisted under one lock, so a logout can't be
//!   undone by an in-flight refresh or login exchange.
//! - The token endpoint must be https and redirects are never followed.
use std::fmt;
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
/// Refresh this long before expiry (at most half the token's lifetime).
pub const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;
/// A login started with `begin_login` must complete within this time.
pub const PENDING_LOGIN_TTL_MS: u64 = 10 * 60 * 1000;
/// After a transient refresh failure with a still-valid token, retry no sooner than this.
const REFRESH_RETRY_BACKOFF_MS: u64 = 30 * 1000;

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
    /// Spotify rejected `access_token` with 401: forget it so the next `access_token()`
    /// refreshes. Callers retry the request once. Default: no-op.
    fn invalidate(&self, _access_token: &str) {}
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
/// `http://127.0.0.1:43821/callback?...`). A missing `state` yields
/// `Auth("state mismatch")`; an `error=` parameter yields `BardicError::Auth` with a fixed
/// message (`error_description` is attacker-controllable and never echoed).
///
/// This does not verify `state` against a pending login; [`TokenManager::complete_login`]
/// does.
pub fn parse_callback(callback_url: &str) -> Result<CallbackParams> {
    let url = Url::parse(callback_url)
        .map_err(|e| BardicError::Auth(format!("invalid callback URL: {e}")))?;
    let raw = RawCallback::from_url(&url);
    let state = raw.state.ok_or_else(state_mismatch)?;
    if let Some(err) = raw.error {
        return Err(BardicError::Auth(callback_error_message(&err)));
    }
    let code = raw.code.ok_or_else(missing_code)?;
    Ok(CallbackParams { code, state })
}

/// Query parameters of a redirect. `error_description` is deliberately not read.
struct RawCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl RawCallback {
    fn from_url(url: &Url) -> Self {
        let mut raw = RawCallback {
            code: None,
            state: None,
            error: None,
        };
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => raw.code = Some(v.into_owned()),
                "state" => raw.state = Some(v.into_owned()),
                "error" => raw.error = Some(v.into_owned()),
                _ => {}
            }
        }
        raw.code = raw.code.filter(|c| !c.is_empty());
        raw
    }
}

/// Map an OAuth `error` code to a fixed, user-safe message.
fn callback_error_message(error: &str) -> String {
    if error == "access_denied" {
        return "Spotify login was cancelled".into();
    }
    let well_formed = (1..=40).contains(&error.len())
        && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    let code = if well_formed { error } else { "unknown" };
    format!("Spotify login failed ({code})")
}

fn state_mismatch() -> BardicError {
    BardicError::Auth("state mismatch".into())
}

fn missing_code() -> BardicError {
    BardicError::Auth("missing authorization code".into())
}

fn login_expired() -> BardicError {
    BardicError::Auth("login expired, start again".into())
}

/// Constant-time string comparison (length is not secret).
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Strip control characters (terminal escapes) and cap length of server-provided text.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}

#[derive(Serialize, Deserialize)]
struct PendingLogin {
    verifier: String,
    state: String,
    /// Wall-clock ms (see `Clock`) when `begin_login` ran.
    created_at_ms: u64,
}

impl PendingLogin {
    fn is_expired(&self, now_ms: u64) -> bool {
        self.created_at_ms.abs_diff(now_ms) > PENDING_LOGIN_TTL_MS
    }

    fn is_well_formed(&self) -> bool {
        (43..=128).contains(&self.verifier.len())
            && self.verifier.bytes().all(|b| UNRESERVED.contains(&b))
            && self.state.len() == STATE_LEN
            && self.state.bytes().all(|b| URL_SAFE.contains(&b))
    }
}

#[derive(Default)]
struct Cache {
    /// Whether `tokens` reflects the store (lazy load on first use).
    loaded: bool,
    tokens: Option<Tokens>,
    /// Lifetime of `tokens.access_token` as issued, when known (not for loaded tokens).
    lifetime_ms: Option<u64>,
    /// After a transient refresh failure, don't retry before this (while the token is valid).
    retry_refresh_at_ms: u64,
    /// Bumped whenever the token set is replaced (login, logout, forced logout), so an
    /// in-flight refresh can't overwrite it.
    generation: u64,
    /// Bumped only by an explicit logout, so an in-flight login exchange can't resurrect.
    logout_epoch: u64,
}

impl Cache {
    fn set(&mut self, tokens: Option<Tokens>, lifetime_ms: Option<u64>) {
        self.loaded = true;
        self.tokens = tokens;
        self.lifetime_ms = lifetime_ms;
        self.retry_refresh_at_ms = 0;
    }
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
    /// Network failure, 5xx or 429: worth retrying later.
    Transient(BardicError),
    Other(BardicError),
}

impl From<BardicError> for TokenEndpointError {
    fn from(e: BardicError) -> Self {
        TokenEndpointError::Other(e)
    }
}

/// Owns the PKCE login flow and refreshes tokens automatically.
///
/// Concurrency: every "check generation → update cache → write the store" sequence runs
/// under one persistence mutex, so a logout or login can never be undone by a refresh (or
/// login exchange) that was in flight when it happened. The store is never touched while
/// the `cache` mutex is held.
pub struct TokenManager {
    config: AuthConfig,
    store: Arc<dyn TokenStore>,
    clock: Arc<dyn Clock>,
    http: std::result::Result<reqwest::Client, String>,
    pending: Mutex<Option<PendingLogin>>,
    cache: Mutex<Cache>,
    /// Serializes cache+store mutations (lock order: persist → pending → cache).
    persist: Mutex<()>,
    /// Serializes refreshes (single-flight).
    refresh_lock: tokio::sync::Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Consistent view of the cache.
struct Snapshot {
    generation: u64,
    tokens: Option<Tokens>,
    lifetime_ms: Option<u64>,
    retry_refresh_at_ms: u64,
}

impl Snapshot {
    fn of(c: &Cache) -> Self {
        Self {
            generation: c.generation,
            tokens: c.tokens.clone(),
            lifetime_ms: c.lifetime_ms,
            retry_refresh_at_ms: c.retry_refresh_at_ms,
        }
    }
}

impl TokenManager {
    pub fn new(config: AuthConfig, store: Arc<dyn TokenStore>, clock: Arc<dyn Clock>) -> Self {
        // Never follow redirects from the token endpoint: the form carries secrets.
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string());
        if let Err(e) = &http {
            tracing::error!(error = %e, "cannot build token HTTP client");
        }
        Self {
            config,
            store,
            clock,
            http,
            pending: Mutex::new(None),
            cache: Mutex::new(Cache::default()),
            persist: Mutex::new(()),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Accounts endpoint URL; must be https (plain http only for loopback test servers).
    fn accounts_endpoint(&self, path: &str) -> Result<Url> {
        let url = Url::parse(&self.config.accounts_url(path))
            .map_err(|e| BardicError::Invalid(format!("accounts base URL: {e}")))?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        match url.scheme() {
            "https" => Ok(url),
            "http" if loopback => Ok(url),
            _ => Err(BardicError::Invalid(
                "accounts base URL must use https".into(),
            )),
        }
    }

    /// Generate verifier/challenge + random `state`, remember them, and return the
    /// `/authorize` URL the UI should open (browser / Custom Tab). The pending login
    /// expires after [`PENDING_LOGIN_TTL_MS`].
    pub fn begin_login(&self) -> Result<String> {
        let verifier = random_string(UNRESERVED, VERIFIER_LEN);
        let state = random_string(URL_SAFE, STATE_LEN);
        let challenge = pkce_challenge(&verifier);

        let mut url = self.accounts_endpoint("/authorize")?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &self.config.redirect_uri)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge)
            .append_pair("state", &state)
            .append_pair("scope", &self.config.scopes.join(" "));

        *lock(&self.pending) = Some(PendingLogin {
            verifier,
            state,
            created_at_ms: self.clock.now_ms(),
        });
        tracing::debug!("spotify login started");
        Ok(url.into())
    }

    /// Opaque JSON of the in-flight login (includes the PKCE verifier: store it encrypted),
    /// so Android can survive process death between `begin_login` and the redirect.
    /// `None` if no unexpired login is pending.
    pub fn export_pending(&self) -> Option<String> {
        let pending = lock(&self.pending);
        let p = pending.as_ref()?;
        if p.is_expired(self.clock.now_ms()) {
            return None;
        }
        serde_json::to_string(p).ok()
    }

    /// Restore a login saved with [`Self::export_pending`], replacing any pending one.
    /// Fails with `Auth("login expired, start again")` past the TTL.
    pub fn restore_pending(&self, s: &str) -> Result<()> {
        let p: PendingLogin = serde_json::from_str(s)
            .ok()
            .filter(PendingLogin::is_well_formed)
            .ok_or_else(|| BardicError::Invalid("pending login data is invalid".into()))?;
        if p.is_expired(self.clock.now_ms()) {
            return Err(login_expired());
        }
        *lock(&self.pending) = Some(p);
        Ok(())
    }

    /// Resolve the callback (a bare `/callback?..` is resolved against the redirect URI)
    /// and require scheme, host, port and path to equal the configured redirect URI.
    fn callback_url(&self, callback_url: &str) -> Result<Url> {
        let expected = Url::parse(&self.config.redirect_uri)
            .map_err(|e| BardicError::Invalid(format!("redirect URI: {e}")))?;
        let invalid = |e: url::ParseError| BardicError::Auth(format!("invalid callback URL: {e}"));
        let url = match Url::parse(callback_url) {
            Ok(u) => u,
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                expected.join(callback_url).map_err(invalid)?
            }
            Err(e) => return Err(invalid(e)),
        };
        if url.scheme() != expected.scheme()
            || url.host_str() != expected.host_str()
            || url.port_or_known_default() != expected.port_or_known_default()
            || url.path() != expected.path()
        {
            return Err(BardicError::Auth(
                "callback URL does not match the redirect URI".into(),
            ));
        }
        Ok(url)
    }

    /// Exchange the code from the redirect. `callback_url` is the full redirect URL
    /// (`...?code=..&state=..` or `?error=..`); state must match the pending login.
    /// A bare path+query (`/callback?code=..`, as seen by a loopback HTTP listener) is
    /// resolved against the configured redirect URI.
    ///
    /// Callbacks whose state is missing or wrong fail with `Auth("state mismatch")` and
    /// leave the pending login intact (even if they carry `error=`), so a forged request
    /// can't cancel the real login.
    pub async fn complete_login(&self, callback_url: &str) -> Result<()> {
        let url = self.callback_url(callback_url)?;
        let raw = RawCallback::from_url(&url);
        let state = raw.state.as_deref().ok_or_else(state_mismatch)?;

        // Take the pending login only if the state matches. Taking it makes the verifier
        // single-use. The logout epoch is read under the same lock (logout clears pending
        // under it too), so a logout during the exchange is always detected.
        let (verifier, code, logout_epoch) = {
            let mut pending = lock(&self.pending);
            match pending.as_ref() {
                None => return Err(BardicError::Auth("no login in progress".into())),
                Some(p) if p.is_expired(self.clock.now_ms()) => {
                    *pending = None;
                    return Err(login_expired());
                }
                Some(p) if !ct_eq(&p.state, state) => {
                    tracing::warn!("spotify login callback with mismatched state");
                    return Err(state_mismatch());
                }
                Some(_) => {}
            }
            if let Some(err) = raw.error.as_deref() {
                *pending = None;
                let msg = callback_error_message(err);
                tracing::info!(reason = %msg, "spotify login not completed");
                return Err(BardicError::Auth(msg));
            }
            let code = raw.code.ok_or_else(missing_code)?;
            let epoch = lock(&self.cache).logout_epoch;
            let p = pending.take().ok_or_else(state_mismatch)?;
            (p.verifier, code, epoch)
        };

        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
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
            Err(TokenEndpointError::Transient(e) | TokenEndpointError::Other(e)) => return Err(e),
        };
        let refresh_token = resp
            .refresh_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| BardicError::Auth("token response missing refresh_token".into()))?;
        let lifetime_ms = resp.expires_in.saturating_mul(1000);
        let tokens = Tokens {
            access_token: resp.access_token,
            refresh_token,
            expires_at_ms: self.expiry(resp.expires_in),
            scope: resp.scope.unwrap_or_default(),
        };

        let _persist = lock(&self.persist);
        {
            let mut cache = lock(&self.cache);
            if cache.logout_epoch != logout_epoch {
                tracing::info!("spotify login discarded: logged out during the exchange");
                return Err(BardicError::Auth("login cancelled by logout".into()));
            }
            cache.generation += 1;
            cache.set(Some(tokens.clone()), Some(lifetime_ms));
        }
        tracing::info!(
            expires_at_ms = tokens.expires_at_ms,
            "spotify login completed"
        );
        self.store.save(&tokens)
    }

    /// True if refresh/access tokens are stored.
    pub fn is_logged_in(&self) -> bool {
        matches!(
            self.snapshot(),
            Ok(Snapshot {
                tokens: Some(_),
                ..
            })
        )
    }

    pub fn logout(&self) -> Result<()> {
        let _persist = lock(&self.persist);
        {
            let mut pending = lock(&self.pending);
            let mut cache = lock(&self.cache);
            cache.generation += 1;
            cache.logout_epoch += 1;
            cache.set(None, None);
            *pending = None;
        }
        tracing::info!("spotify logout");
        self.store.clear()
    }

    /// Mark `access_token` as expired so the next [`AccessTokenProvider::access_token`]
    /// refreshes. No-op if a different token is cached (e.g. it was already refreshed).
    ///
    /// Callers should call this when Spotify answers 401 for a request made with
    /// `access_token`, then retry that request once with a fresh token.
    pub fn invalidate(&self, access_token: &str) {
        let mut cache = lock(&self.cache);
        if let Some(t) = cache.tokens.as_mut() {
            if ct_eq(&t.access_token, access_token) {
                t.expires_at_ms = 0;
                cache.retry_refresh_at_ms = 0;
                tracing::debug!("spotify access token invalidated");
            }
        }
    }

    fn expiry(&self, expires_in_secs: u64) -> u64 {
        self.clock
            .now_ms()
            .saturating_add(expires_in_secs.saturating_mul(1000))
    }

    /// Cached state, loading tokens from the store on first use (outside the cache lock).
    fn snapshot(&self) -> Result<Snapshot> {
        {
            let cache = lock(&self.cache);
            if cache.loaded {
                return Ok(Snapshot::of(&cache));
            }
        }
        let _persist = lock(&self.persist);
        if lock(&self.cache).loaded {
            return Ok(Snapshot::of(&lock(&self.cache)));
        }
        // Token mutations all hold `persist`, so nothing can change the cache meanwhile.
        let loaded = self.store.load()?;
        let mut cache = lock(&self.cache);
        cache.set(loaded, None);
        Ok(Snapshot::of(&cache))
    }

    /// Usable without refreshing: fresh, or valid and backing off after a transient error.
    fn usable(&self, s: &Snapshot, t: &Tokens) -> bool {
        let now = self.clock.now_ms();
        let margin = s
            .lifetime_ms
            .map_or(REFRESH_MARGIN_MS, |l| REFRESH_MARGIN_MS.min(l / 2));
        now.saturating_add(margin) < t.expires_at_ms
            || (now < s.retry_refresh_at_ms && now < t.expires_at_ms)
    }

    /// After losing a race to login/logout: the current token, or `NotAuthenticated`.
    fn current_after_race(&self) -> Result<String> {
        match &lock(&self.cache).tokens {
            Some(t) => Ok(t.access_token.clone()),
            None => Err(BardicError::NotAuthenticated),
        }
    }

    async fn refresh(&self, generation: u64, old: Tokens) -> Result<String> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", old.refresh_token.as_str()),
            ("client_id", self.config.client_id.as_str()),
        ];
        tracing::debug!("refreshing spotify access token");
        let resp = match self.token_request(&form).await {
            Ok(r) => r,
            Err(TokenEndpointError::InvalidGrant) => {
                return if self.clear_if_generation(generation)? {
                    tracing::warn!("spotify refresh token rejected; logged out");
                    Err(BardicError::NotAuthenticated)
                } else {
                    self.current_after_race()
                };
            }
            Err(TokenEndpointError::Transient(e)) => {
                let now = self.clock.now_ms();
                let mut cache = lock(&self.cache);
                if cache.generation != generation {
                    drop(cache);
                    return self.current_after_race();
                }
                let current = match &cache.tokens {
                    // Still valid: keep using it, retry the refresh a bit later.
                    Some(t) if now < t.expires_at_ms => t.access_token.clone(),
                    _ => return Err(e),
                };
                tracing::warn!(error = %e, "spotify token refresh failed; using current token");
                cache.retry_refresh_at_ms = now.saturating_add(REFRESH_RETRY_BACKOFF_MS);
                return Ok(current);
            }
            Err(TokenEndpointError::Other(e)) => return Err(e),
        };
        let lifetime_ms = resp.expires_in.saturating_mul(1000);
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

        let _persist = lock(&self.persist);
        {
            let mut cache = lock(&self.cache);
            // A logout/login happened during the request: don't overwrite it.
            if cache.generation != generation {
                drop(cache);
                return self.current_after_race();
            }
            cache.set(Some(tokens.clone()), Some(lifetime_ms));
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

    /// Forced logout after `invalid_grant`: clears tokens only if nothing replaced them
    /// since `generation`. Leaves any pending login alone. Returns whether it cleared.
    fn clear_if_generation(&self, generation: u64) -> Result<bool> {
        let _persist = lock(&self.persist);
        {
            let mut cache = lock(&self.cache);
            if cache.generation != generation {
                return Ok(false);
            }
            cache.generation += 1;
            cache.set(None, None);
        }
        self.store.clear()?;
        Ok(true)
    }

    async fn token_request(
        &self,
        form: &[(&str, &str)],
    ) -> std::result::Result<TokenResponse, TokenEndpointError> {
        let http = self
            .http
            .as_ref()
            .map_err(|e| BardicError::Invalid(format!("token HTTP client: {e}")))?;
        let url = self.accounts_endpoint("/api/token")?;
        let network = |e: reqwest::Error| {
            TokenEndpointError::Transient(BardicError::Network(e.without_url().to_string()))
        };
        let resp = http.post(url).form(form).send().await.map_err(network)?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(network)?;
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
            }) => format!(
                "token endpoint returned {status}: {} ({})",
                sanitize(&error),
                sanitize(&d)
            ),
            Some(TokenErrorResponse {
                error,
                error_description: None,
            }) => format!("token endpoint returned {status}: {}", sanitize(&error)),
            None => format!("token endpoint returned {status}"),
        };
        let e = BardicError::Auth(msg);
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            Err(TokenEndpointError::Transient(e))
        } else {
            Err(TokenEndpointError::Other(e))
        }
    }
}

#[async_trait]
impl AccessTokenProvider for TokenManager {
    /// Returns the stored access token, refreshing first if it expires within
    /// `REFRESH_MARGIN_MS` (or half its lifetime, if shorter). Concurrent callers share a
    /// single refresh request. If a refresh fails transiently (network, 5xx, 429) while the
    /// current token is still valid, that token is returned and the refresh is retried
    /// later. On a Spotify 401, call [`TokenManager::invalidate`] and retry once.
    async fn access_token(&self) -> Result<String> {
        let s = self.snapshot()?;
        match &s.tokens {
            None => return Err(BardicError::NotAuthenticated),
            Some(t) if self.usable(&s, t) => return Ok(t.access_token.clone()),
            Some(_) => {}
        }
        let _guard = self.refresh_lock.lock().await;
        // Re-check: another caller may have refreshed (or logged out) while we waited.
        let s = self.snapshot()?;
        match s.tokens.clone() {
            None => Err(BardicError::NotAuthenticated),
            Some(t) if self.usable(&s, &t) => Ok(t.access_token),
            Some(t) => self.refresh(s.generation, t).await,
        }
    }

    fn invalidate(&self, access_token: &str) {
        TokenManager::invalidate(self, access_token)
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
            Err(BardicError::Auth(m)) if m == "Spotify login was cancelled"
        ));
        assert!(matches!(
            parse_callback("bardic://callback?error=server_error&error_description=evil&state=x"),
            Err(BardicError::Auth(m)) if m == "Spotify login failed (server_error)"
        ));
        assert!(matches!(
            parse_callback("bardic://callback?code=abc"),
            Err(BardicError::Auth(m)) if m == "state mismatch"
        ));
        assert!(matches!(
            parse_callback("bardic://callback?error=access_denied"),
            Err(BardicError::Auth(m)) if m == "state mismatch"
        ));
    }

    #[test]
    fn callback_error_codes_are_whitelisted() {
        assert_eq!(
            callback_error_message("access_denied"),
            "Spotify login was cancelled"
        );
        assert_eq!(
            callback_error_message("invalid_scope"),
            "Spotify login failed (invalid_scope)"
        );
        for bad in ["", "Evil", "a b", "\x1b[31m", &"a".repeat(41)] {
            assert_eq!(
                callback_error_message(bad),
                "Spotify login failed (unknown)"
            );
        }
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn sanitize_strips_control_chars() {
        assert_eq!(sanitize("a\x1b[2Jb\nc"), "a[2Jbc");
        assert_eq!(sanitize(&"x".repeat(500)).len(), 200);
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
