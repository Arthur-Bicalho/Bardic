use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bardic_core::auth::{
    pkce_challenge, AccessTokenProvider, AuthConfig, MemoryTokenStore, TokenManager, TokenStore,
    Tokens, DEFAULT_SCOPES, PENDING_LOGIN_TTL_MS, REFRESH_MARGIN_MS,
};
use bardic_core::clock::MockClock;
use bardic_core::BardicError;
use serde_json::json;
use url::Url;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const T0: u64 = 1_700_000_000_000;
const REDIRECT: &str = "http://127.0.0.1:43821/callback";

struct Fixture {
    server: MockServer,
    store: Arc<MemoryTokenStore>,
    clock: MockClock,
    mgr: Arc<TokenManager>,
}

async fn fixture() -> Fixture {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryTokenStore::default());
    let clock = MockClock::new(T0);
    let mut cfg = AuthConfig::new("client-123", REDIRECT);
    cfg.accounts_base_url = server.uri();
    let mgr = Arc::new(TokenManager::new(
        cfg,
        store.clone(),
        Arc::new(clock.clone()),
    ));
    Fixture {
        server,
        store,
        clock,
        mgr,
    }
}

fn query(url: &str) -> HashMap<String, String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

fn form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn stored(access: &str, refresh: &str, expires_at_ms: u64) -> Tokens {
    Tokens {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at_ms,
        scope: "user-read-playback-state".into(),
    }
}

fn token_ok(access: &str, refresh: Option<&str>) -> ResponseTemplate {
    let mut body = json!({
        "access_token": access,
        "token_type": "Bearer",
        "scope": "user-read-playback-state user-modify-playback-state",
        "expires_in": 3600,
    });
    if let Some(r) = refresh {
        body["refresh_token"] = json!(r);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

#[test]
fn pkce_rfc7636_vector() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[tokio::test]
async fn authorize_url_has_all_params() {
    let f = fixture().await;
    let url = f.mgr.begin_login().unwrap();
    let parsed = Url::parse(&url).unwrap();
    assert_eq!(parsed.path(), "/authorize");
    let q = query(&url);
    assert_eq!(q["client_id"], "client-123");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["redirect_uri"], REDIRECT);
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["code_challenge"].len(), 43);
    assert_eq!(q["state"].len(), 32);
    assert_eq!(q["scope"], DEFAULT_SCOPES.join(" "));
    // Spaces in scope must be encoded, not raw.
    assert!(!url.contains(' '));

    // Fresh randomness each time.
    let q2 = query(&f.mgr.begin_login().unwrap());
    assert_ne!(q["state"], q2["state"]);
    assert_ne!(q["code_challenge"], q2["code_challenge"]);
}

#[tokio::test]
async fn complete_login_happy_path() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains("client_id=client-123"))
        .and(body_string_contains(
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A43821%2Fcallback",
        ))
        .and(body_string_contains("code_verifier="))
        .respond_with(token_ok("AT1", Some("RT1")))
        .expect(1)
        .mount(&f.server)
        .await;

    let q = query(&f.mgr.begin_login().unwrap());
    assert!(!f.mgr.is_logged_in());
    f.mgr
        .complete_login(&format!("{REDIRECT}?code=the-code&state={}", q["state"]))
        .await
        .unwrap();

    // challenge in the authorize URL == S256(verifier sent to the token endpoint)
    let reqs = f.server.received_requests().await.unwrap();
    let sent = form(&reqs[0].body);
    let verifier = &sent["code_verifier"];
    assert_eq!(verifier.len(), 64);
    assert!(verifier
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)));
    assert_eq!(pkce_challenge(verifier), q["code_challenge"]);

    let t = f.store.load().unwrap().unwrap();
    assert_eq!(t.access_token, "AT1");
    assert_eq!(t.refresh_token, "RT1");
    assert_eq!(t.expires_at_ms, T0 + 3_600_000);
    assert_eq!(
        t.scope,
        "user-read-playback-state user-modify-playback-state"
    );
    assert!(f.mgr.is_logged_in());
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT1");

    // Pending login is consumed: replaying the callback fails.
    let replay = f
        .mgr
        .complete_login(&format!("{REDIRECT}?code=the-code&state={}", q["state"]))
        .await;
    assert!(matches!(replay, Err(BardicError::Auth(_))));
}

#[tokio::test]
async fn new_begin_login_replaces_pending() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(token_ok("AT1", Some("RT1")))
        .expect(0)
        .mount(&f.server)
        .await;
    let old = query(&f.mgr.begin_login().unwrap());
    let _new = f.mgr.begin_login().unwrap();
    let r = f
        .mgr
        .complete_login(&format!("{REDIRECT}?code=c&state={}", old["state"]))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "state mismatch"));
}

#[tokio::test]
async fn state_mismatch_is_rejected() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("code=genuine"))
        .respond_with(token_ok("AT1", Some("RT1")))
        .expect(1)
        .mount(&f.server)
        .await;
    let q = query(&f.mgr.begin_login().unwrap());
    let r = f
        .mgr
        .complete_login(&format!("{REDIRECT}?code=forged&state=evil"))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "state mismatch"));
    let r = f
        .mgr
        .complete_login(&format!("{REDIRECT}?code=forged"))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "state mismatch"));
    assert!(!f.mgr.is_logged_in());
    // A forged callback doesn't cancel the genuine pending login.
    f.mgr
        .complete_login(&format!("{REDIRECT}?code=genuine&state={}", q["state"]))
        .await
        .unwrap();
    assert!(f.mgr.is_logged_in());
}

#[tokio::test]
async fn no_pending_login_is_rejected() {
    let f = fixture().await;
    let r = f
        .mgr
        .complete_login(&format!("{REDIRECT}?code=c&state=s"))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(_))));
}

#[tokio::test]
async fn error_param_is_auth_error() {
    let f = fixture().await;
    let q = query(&f.mgr.begin_login().unwrap());
    let r = f
        .mgr
        .complete_login(&format!(
            "{REDIRECT}?error=access_denied&state={}",
            q["state"]
        ))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "Spotify login was cancelled"));
    assert!(!f.mgr.is_logged_in());
    // The matching error callback consumed the pending login.
    let r = f
        .mgr
        .complete_login(&format!("{REDIRECT}?code=c&state={}", q["state"]))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "no login in progress"));
}

#[tokio::test]
async fn token_endpoint_error_on_login_is_auth_error() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_client"})))
        .mount(&f.server)
        .await;
    let q = query(&f.mgr.begin_login().unwrap());
    let r = f
        .mgr
        .complete_login(&format!("{REDIRECT}?code=c&state={}", q["state"]))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m.contains("invalid_client")));
}

#[tokio::test]
async fn not_logged_in_is_not_authenticated() {
    let f = fixture().await;
    assert!(!f.mgr.is_logged_in());
    assert!(matches!(
        f.mgr.access_token().await,
        Err(BardicError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn returns_cached_token_before_margin() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(token_ok("NEW", None))
        .expect(0)
        .mount(&f.server)
        .await;
    let expires = T0 + 3_600_000;
    f.store.save(&stored("OLD", "RT", expires)).unwrap();
    assert!(f.mgr.is_logged_in());
    assert_eq!(f.mgr.access_token().await.unwrap(), "OLD");
    // Just outside the margin: still cached.
    f.clock.set_ms(expires - REFRESH_MARGIN_MS - 1);
    assert_eq!(f.mgr.access_token().await.unwrap(), "OLD");
}

#[tokio::test]
async fn refreshes_inside_margin_and_keeps_refresh_token() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=RT-old"))
        .and(body_string_contains("client_id=client-123"))
        .respond_with(token_ok("AT-new", None))
        .expect(1)
        .mount(&f.server)
        .await;
    let expires = T0 + 3_600_000;
    f.store.save(&stored("AT-old", "RT-old", expires)).unwrap();
    f.clock.set_ms(expires - REFRESH_MARGIN_MS);

    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-new");
    let t = f.store.load().unwrap().unwrap();
    assert_eq!(t.access_token, "AT-new");
    assert_eq!(t.refresh_token, "RT-old");
    assert_eq!(t.expires_at_ms, expires - REFRESH_MARGIN_MS + 3_600_000);
    // Now cached again.
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-new");
}

#[tokio::test]
async fn refresh_uses_rotated_refresh_token() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(token_ok("AT-new", Some("RT-new")))
        .expect(1)
        .mount(&f.server)
        .await;
    f.store.save(&stored("AT-old", "RT-old", T0)).unwrap();
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-new");
    assert_eq!(f.store.load().unwrap().unwrap().refresh_token, "RT-new");
}

#[tokio::test]
async fn invalid_grant_logs_out() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error": "invalid_grant", "error_description": "Refresh token revoked"}),
        ))
        .expect(1)
        .mount(&f.server)
        .await;
    f.store.save(&stored("AT", "RT", T0)).unwrap();
    assert!(matches!(
        f.mgr.access_token().await,
        Err(BardicError::NotAuthenticated)
    ));
    assert!(f.store.load().unwrap().is_none());
    assert!(!f.mgr.is_logged_in());
    assert!(matches!(
        f.mgr.access_token().await,
        Err(BardicError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn other_refresh_errors_are_auth_and_keep_tokens() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.server)
        .await;
    f.store.save(&stored("AT", "RT", T0)).unwrap();
    assert!(matches!(
        f.mgr.access_token().await,
        Err(BardicError::Auth(_))
    ));
    assert!(f.mgr.is_logged_in());
}

#[tokio::test]
async fn transport_error_is_network() {
    let store = Arc::new(MemoryTokenStore::default());
    store.save(&stored("AT", "RT", T0)).unwrap();
    // A port with nothing listening.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cfg = AuthConfig::new("client-123", REDIRECT);
    cfg.accounts_base_url = format!("http://127.0.0.1:{port}");
    let mgr = TokenManager::new(cfg, store, Arc::new(MockClock::new(T0)));
    assert!(matches!(
        mgr.access_token().await,
        Err(BardicError::Network(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_share_one_refresh() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(token_ok("AT-new", None).set_delay(Duration::from_millis(200)))
        .expect(1)
        .mount(&f.server)
        .await;
    f.store.save(&stored("AT-old", "RT", T0)).unwrap();

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let mgr = f.mgr.clone();
            tokio::spawn(async move { mgr.access_token().await })
        })
        .collect();
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap(), "AT-new");
    }
    // `expect(1)` is verified when the server drops.
}

#[tokio::test]
async fn logout_clears_everything() {
    let f = fixture().await;
    f.store.save(&stored("AT", "RT", T0 + 3_600_000)).unwrap();
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT");
    f.mgr.logout().unwrap();
    assert!(!f.mgr.is_logged_in());
    assert!(f.store.load().unwrap().is_none());
    assert!(matches!(
        f.mgr.access_token().await,
        Err(BardicError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn relative_callback_from_loopback_listener() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(token_ok("AT1", Some("RT1")))
        .expect(1)
        .mount(&f.server)
        .await;
    let q = query(&f.mgr.begin_login().unwrap());
    f.mgr
        .complete_login(&format!("/callback?code=c&state={}", q["state"]))
        .await
        .unwrap();
    assert!(f.mgr.is_logged_in());
}

// ---------------------------------------------------------------- hardening / races

/// Store whose `save` of one specific access token is slow, to widen race windows.
struct SlowStore {
    inner: MemoryTokenStore,
    slow_access_token: &'static str,
}

impl TokenStore for SlowStore {
    fn load(&self) -> bardic_core::Result<Option<Tokens>> {
        self.inner.load()
    }
    fn save(&self, t: &Tokens) -> bardic_core::Result<()> {
        if t.access_token == self.slow_access_token {
            std::thread::sleep(Duration::from_millis(400));
        }
        self.inner.save(t)
    }
    fn clear(&self) -> bardic_core::Result<()> {
        self.inner.clear()
    }
}

fn manager(
    server: &MockServer,
    store: Arc<dyn TokenStore>,
    clock: &MockClock,
) -> Arc<TokenManager> {
    let mut cfg = AuthConfig::new("client-123", REDIRECT);
    cfg.accounts_base_url = server.uri();
    Arc::new(TokenManager::new(cfg, store, Arc::new(clock.clone())))
}

fn slow_store(slow: &'static str) -> Arc<SlowStore> {
    let s = Arc::new(SlowStore {
        inner: MemoryTokenStore::default(),
        slow_access_token: slow,
    });
    s.inner.save(&stored("AT-old", "RT-old", T0)).unwrap();
    s
}

async fn mount_refresh(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(resp)
        .mount(server)
        .await;
}

async fn mount_code(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(resp)
        .mount(server)
        .await;
}

fn callback(state: &str) -> String {
    format!("{REDIRECT}?code=the-code&state={state}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_during_refresh_request_does_not_resurrect() {
    let server = MockServer::start().await;
    mount_refresh(
        &server,
        token_ok("AT-refreshed", None).set_delay(Duration::from_millis(300)),
    )
    .await;
    let store = slow_store("none");
    let mgr = manager(&server, store.clone(), &MockClock::new(T0));
    let m = mgr.clone();
    let h = tokio::spawn(async move { m.access_token().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    mgr.logout().unwrap();
    assert!(matches!(
        h.await.unwrap(),
        Err(BardicError::NotAuthenticated)
    ));
    assert!(store.load().unwrap().is_none());
    assert!(!mgr.is_logged_in());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_during_refresh_save_does_not_resurrect() {
    let server = MockServer::start().await;
    mount_refresh(&server, token_ok("AT-refreshed", None)).await;
    let store = slow_store("AT-refreshed");
    let mgr = manager(&server, store.clone(), &MockClock::new(T0));
    let m = mgr.clone();
    let h = tokio::spawn(async move { m.access_token().await });
    tokio::time::sleep(Duration::from_millis(150)).await; // refresh is now inside save()
    let m = mgr.clone();
    tokio::task::spawn_blocking(move || m.logout())
        .await
        .unwrap()
        .unwrap();
    let _ = h.await.unwrap();
    assert!(
        store.load().unwrap().is_none(),
        "refresh resurrected tokens"
    );
    assert!(!mgr.is_logged_in());
    assert!(matches!(
        mgr.access_token().await,
        Err(BardicError::NotAuthenticated)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_login_during_refresh_request_wins() {
    let server = MockServer::start().await;
    mount_refresh(
        &server,
        token_ok("AT-refreshed", None).set_delay(Duration::from_millis(300)),
    )
    .await;
    mount_code(&server, token_ok("AT-login", Some("RT-login"))).await;
    let store = slow_store("none");
    let mgr = manager(&server, store.clone(), &MockClock::new(T0));
    let q = query(&mgr.begin_login().unwrap());
    let m = mgr.clone();
    let h = tokio::spawn(async move { m.access_token().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    mgr.complete_login(&callback(&q["state"])).await.unwrap();
    assert_eq!(h.await.unwrap().unwrap(), "AT-login");
    let t = store.load().unwrap().unwrap();
    assert_eq!(
        (t.access_token.as_str(), t.refresh_token.as_str()),
        ("AT-login", "RT-login")
    );
    assert_eq!(mgr.access_token().await.unwrap(), "AT-login");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_login_during_refresh_save_wins() {
    let server = MockServer::start().await;
    mount_refresh(&server, token_ok("AT-refreshed", None)).await;
    mount_code(&server, token_ok("AT-login", Some("RT-login"))).await;
    let store = slow_store("AT-refreshed");
    let mgr = manager(&server, store.clone(), &MockClock::new(T0));
    let q = query(&mgr.begin_login().unwrap());
    let m = mgr.clone();
    let h = tokio::spawn(async move { m.access_token().await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    mgr.complete_login(&callback(&q["state"])).await.unwrap();
    let _ = h.await.unwrap();
    assert_eq!(store.load().unwrap().unwrap().access_token, "AT-login");
    assert_eq!(mgr.access_token().await.unwrap(), "AT-login");
}

fn invalid_grant() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"}))
}

#[tokio::test]
async fn pending_login_survives_auto_logout() {
    let f = fixture().await;
    mount_refresh(&f.server, invalid_grant()).await;
    mount_code(&f.server, token_ok("AT-login", Some("RT-login"))).await;
    f.store.save(&stored("AT", "RT", T0)).unwrap();
    let q = query(&f.mgr.begin_login().unwrap());
    assert!(matches!(
        f.mgr.access_token().await,
        Err(BardicError::NotAuthenticated)
    ));
    assert!(f.store.load().unwrap().is_none());
    f.mgr.complete_login(&callback(&q["state"])).await.unwrap();
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-login");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_during_invalid_grant_refresh_is_not_wiped() {
    let server = MockServer::start().await;
    mount_refresh(
        &server,
        invalid_grant().set_delay(Duration::from_millis(300)),
    )
    .await;
    mount_code(&server, token_ok("AT-login", Some("RT-login"))).await;
    let store = slow_store("none");
    let mgr = manager(&server, store.clone(), &MockClock::new(T0));
    let q = query(&mgr.begin_login().unwrap());
    let m = mgr.clone();
    let h = tokio::spawn(async move { m.access_token().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    mgr.complete_login(&callback(&q["state"])).await.unwrap();
    assert_eq!(h.await.unwrap().unwrap(), "AT-login");
    assert_eq!(store.load().unwrap().unwrap().access_token, "AT-login");
    assert!(mgr.is_logged_in());
}

#[tokio::test]
async fn error_callback_with_wrong_or_missing_state_is_ignored() {
    let f = fixture().await;
    mount_code(&f.server, token_ok("AT1", Some("RT1"))).await;
    let q = query(&f.mgr.begin_login().unwrap());
    for forged in [
        format!("{REDIRECT}?error=access_denied&state=evil"),
        format!("{REDIRECT}?error=access_denied"),
        format!("{REDIRECT}?error=access_denied&error_description=Visit+evil.example"),
    ] {
        let r = f.mgr.complete_login(&forged).await;
        assert!(
            matches!(r, Err(BardicError::Auth(m)) if m == "state mismatch"),
            "{forged}"
        );
    }
    // The genuine login was not cancelled.
    f.mgr.complete_login(&callback(&q["state"])).await.unwrap();
    assert!(f.mgr.is_logged_in());
}

#[tokio::test]
async fn error_description_is_never_echoed() {
    let f = fixture().await;
    let q = query(&f.mgr.begin_login().unwrap());
    let r = f
        .mgr
        .complete_login(&format!(
            "{REDIRECT}?error=server_error&error_description=%1B%5B2JCall+0800-EVIL&state={}",
            q["state"]
        ))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "Spotify login failed (server_error)"));

    let q = query(&f.mgr.begin_login().unwrap());
    let r = f
        .mgr
        .complete_login(&format!(
            "{REDIRECT}?error=%1B%5B31mEVIL&state={}",
            q["state"]
        ))
        .await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "Spotify login failed (unknown)"));
}

#[tokio::test]
async fn callback_must_match_redirect_uri() {
    let f = fixture().await;
    mount_code(&f.server, token_ok("AT1", Some("RT1"))).await;
    let q = query(&f.mgr.begin_login().unwrap());
    let s = &q["state"];
    for bad in [
        format!("http://127.0.0.1:43822/callback?code=c&state={s}"),
        format!("http://localhost:43821/callback?code=c&state={s}"),
        format!("https://127.0.0.1:43821/callback?code=c&state={s}"),
        format!("http://127.0.0.1:43821/callbackx?code=c&state={s}"),
        format!("http://127.0.0.1:43821/other?code=c&state={s}"),
        format!("bardic://callback?code=c&state={s}"),
        format!("/callback/x?code=c&state={s}"),
        format!("//evil.example/callback?code=c&state={s}"),
    ] {
        let r = f.mgr.complete_login(&bad).await;
        assert!(matches!(r, Err(BardicError::Auth(_))), "{bad}");
    }
    assert!(f.server.received_requests().await.unwrap().is_empty());
    // Pending login untouched.
    f.mgr
        .complete_login(&format!("/callback?code=c&state={s}"))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_during_code_exchange_discards_login() {
    let f = fixture().await;
    mount_code(
        &f.server,
        token_ok("AT1", Some("RT1")).set_delay(Duration::from_millis(300)),
    )
    .await;
    let q = query(&f.mgr.begin_login().unwrap());
    let m = f.mgr.clone();
    let url = callback(&q["state"]);
    let h = tokio::spawn(async move { m.complete_login(&url).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.mgr.logout().unwrap();
    assert!(matches!(h.await.unwrap(), Err(BardicError::Auth(_))));
    assert!(f.store.load().unwrap().is_none());
    assert!(!f.mgr.is_logged_in());
}

#[tokio::test]
async fn invalidate_forces_refresh_only_for_that_token() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(token_ok("AT-new", None))
        .expect(1)
        .mount(&f.server)
        .await;
    f.store.save(&stored("AT", "RT", T0 + 3_600_000)).unwrap();
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT");
    f.mgr.invalidate("something-else");
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT");
    // Through the trait, as the engine does.
    AccessTokenProvider::invalidate(&*f.mgr, "AT");
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-new");
    // Stale token from before the refresh: no-op.
    f.mgr.invalidate("AT");
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-new");
}

#[tokio::test]
async fn short_lived_token_is_not_refreshed_on_every_call() {
    let f = fixture().await;
    mount_code(
        &f.server,
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "AT-short", "refresh_token": "RT", "expires_in": 120
        })),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(token_ok("AT-new", None))
        .expect(1)
        .mount(&f.server)
        .await;
    let q = query(&f.mgr.begin_login().unwrap());
    f.mgr.complete_login(&callback(&q["state"])).await.unwrap();
    // Margin is clamped to half the 120 s lifetime.
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-short");
    f.clock.advance_ms(59_000);
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-short");
    f.clock.advance_ms(2_000);
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT-new");
}

#[tokio::test]
async fn transient_refresh_failure_keeps_using_valid_token() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(503))
        .expect(2)
        .mount(&f.server)
        .await;
    // Inside the refresh margin but not expired.
    f.store.save(&stored("AT", "RT", T0 + 60_000)).unwrap();
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT");
    // Backing off: no second request yet.
    assert_eq!(f.mgr.access_token().await.unwrap(), "AT");
    // Expired now: the failure surfaces.
    f.clock.advance_ms(61_000);
    assert!(matches!(
        f.mgr.access_token().await,
        Err(BardicError::Auth(_))
    ));
    assert!(f.mgr.is_logged_in());
}

#[tokio::test]
async fn transient_network_failure_keeps_using_valid_token() {
    let store = Arc::new(MemoryTokenStore::default());
    store.save(&stored("AT", "RT", T0 + 60_000)).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cfg = AuthConfig::new("client-123", REDIRECT);
    cfg.accounts_base_url = format!("http://127.0.0.1:{port}");
    let mgr = TokenManager::new(cfg, store, Arc::new(MockClock::new(T0)));
    assert_eq!(mgr.access_token().await.unwrap(), "AT");
}

#[tokio::test]
async fn non_https_accounts_base_is_rejected() {
    let store = Arc::new(MemoryTokenStore::default());
    store.save(&stored("AT", "RT", T0)).unwrap();
    let mut cfg = AuthConfig::new("client-123", REDIRECT);
    cfg.accounts_base_url = "http://accounts.example.com".into();
    let mgr = TokenManager::new(cfg, store, Arc::new(MockClock::new(T0)));
    assert!(matches!(mgr.begin_login(), Err(BardicError::Invalid(_))));
    assert!(matches!(
        mgr.access_token().await,
        Err(BardicError::Invalid(_))
    ));
}

#[tokio::test]
async fn token_endpoint_redirects_are_not_followed() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/elsewhere", f.server.uri())),
        )
        .mount(&f.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/elsewhere"))
        .respond_with(token_ok("AT-evil", None))
        .expect(0)
        .mount(&f.server)
        .await;
    f.store.save(&stored("AT", "RT", T0)).unwrap();
    assert!(f.mgr.access_token().await.is_err());
}

#[tokio::test]
async fn pending_login_expires() {
    let f = fixture().await;
    let q = query(&f.mgr.begin_login().unwrap());
    f.clock.advance_ms(PENDING_LOGIN_TTL_MS + 1);
    let r = f.mgr.complete_login(&callback(&q["state"])).await;
    assert!(matches!(r, Err(BardicError::Auth(m)) if m == "login expired, start again"));
    assert!(f.mgr.export_pending().is_none());
}

#[tokio::test]
async fn pending_login_export_restore() {
    let f = fixture().await;
    mount_code(&f.server, token_ok("AT1", Some("RT1"))).await;
    assert!(f.mgr.export_pending().is_none());
    let q = query(&f.mgr.begin_login().unwrap());
    let saved = f.mgr.export_pending().unwrap();
    assert!(saved.contains("created_at_ms"));

    // "Process death": a fresh manager restores the in-flight login.
    let store = Arc::new(MemoryTokenStore::default());
    let mgr2 = manager(&f.server, store.clone(), &f.clock);
    mgr2.restore_pending(&saved).unwrap();
    mgr2.complete_login(&callback(&q["state"])).await.unwrap();
    assert_eq!(store.load().unwrap().unwrap().access_token, "AT1");
    // PKCE verifier survived the round trip.
    let reqs = f.server.received_requests().await.unwrap();
    assert_eq!(
        pkce_challenge(&form(&reqs[0].body)["code_verifier"]),
        q["code_challenge"]
    );

    assert!(matches!(
        mgr2.restore_pending("{nope"),
        Err(BardicError::Invalid(_))
    ));
    assert!(matches!(
        mgr2.restore_pending(r#"{"verifier":"x","state":"y","created_at_ms":0}"#),
        Err(BardicError::Invalid(_))
    ));
    f.clock.advance_ms(PENDING_LOGIN_TTL_MS + 1);
    assert!(matches!(
        mgr2.restore_pending(&saved),
        Err(BardicError::Auth(m)) if m == "login expired, start again"
    ));
}
