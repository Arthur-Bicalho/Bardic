//! `BardicCore`: the single object UI layers talk to (Tauri commands now, UniFFI later).
//! It wires the library (SQLite), login (`TokenManager`), the Spotify client and the
//! playback `Engine`, and remembers where the GM currently is (`PlayContext`).
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::runtime::{Handle, Runtime, RuntimeFlavor};

use crate::auth::{AccessTokenProvider, AuthConfig, TokenManager, TokenStore};
use crate::clock::{Clock, SystemClock};
use crate::db::Library;
use crate::engine::{Engine, EngineConfig, EngineObserver, PollHandle, PollMode};
use crate::model::{Id, PlayContext, Resolution, ResolverInput, SearchResults, Trigger};
use crate::resolver::{self, ambience_options};
use crate::spotify::{HttpSpotify, SpotifyApi};
use crate::{BardicError, Result};

pub struct CoreConfig {
    /// SQLite file path; `None` = in-memory (tests / demo). `String` so it crosses FFI.
    pub db_path: Option<String>,
    pub auth: AuthConfig,
    pub engine: EngineConfig,
}

pub struct BardicCore {
    library: Arc<Library>,
    auth: Arc<TokenManager>,
    api: Arc<dyn SpotifyApi>,
    engine: Arc<Engine>,
    context: Mutex<PlayContext>,
    poll: Mutex<Option<PollHandle>>,
    /// Runtime for background work (poll loop). Captured from the caller when one is running
    /// (Tauri, CLI); otherwise owned, because UniFFI calls arrive on plain Kotlin threads.
    handle: Handle,
    _owned_runtime: Option<Runtime>,
}

/// Run a short blocking call (SQLite) from async code without stalling other tasks on
/// this worker. Falls back to a direct call outside a multi-thread runtime.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

impl BardicCore {
    /// Production constructor: real Spotify API and system clock.
    pub fn new(
        config: CoreConfig,
        token_store: Arc<dyn TokenStore>,
        observer: Arc<dyn EngineObserver>,
    ) -> Result<Arc<Self>> {
        Self::with_parts(config, token_store, observer, Arc::new(HttpSpotify::new()), Arc::new(SystemClock))
    }

    /// Constructor with injectable Spotify API and clock (tests).
    pub fn with_parts(
        config: CoreConfig,
        token_store: Arc<dyn TokenStore>,
        observer: Arc<dyn EngineObserver>,
        api: Arc<dyn SpotifyApi>,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>> {
        let library = Arc::new(match config.db_path.as_deref().map(PathBuf::from) {
            Some(p) => {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| BardicError::Db(e.to_string()))?;
                }
                Library::open(&p)?
            }
            None => Library::open_in_memory()?,
        });
        let auth = Arc::new(TokenManager::new(config.auth, token_store, clock.clone()));
        let tokens: Arc<dyn AccessTokenProvider> = auth.clone();
        let engine = Engine::new(api.clone(), tokens, clock, observer, config.engine);
        let (handle, owned) = match Handle::try_current() {
            Ok(h) => (h, None),
            Err(_) => {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("bardic-core")
                    .enable_all()
                    .build()
                    .map_err(|e| BardicError::Invalid(format!("cannot start runtime: {e}")))?;
                (rt.handle().clone(), Some(rt))
            }
        };
        Ok(Arc::new(Self {
            library,
            auth,
            api,
            engine,
            context: Mutex::new(PlayContext::default()),
            poll: Mutex::new(None),
            handle,
            _owned_runtime: owned,
        }))
    }

    // ------------------------------------------------------------ parts

    /// Campaign/chapter/scene/cue/tag/ambience CRUD (synchronous SQLite).
    pub fn library(&self) -> &Arc<Library> {
        &self.library
    }

    /// Transport controls, devices, playback state.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Runtime handle for callers that need to drive async core methods from sync code.
    pub fn runtime(&self) -> &Handle {
        &self.handle
    }

    // ------------------------------------------------------------ auth

    /// URL to open in the browser / Custom Tab.
    pub fn login_url(&self) -> Result<String> {
        self.auth.begin_login()
    }

    /// Pass the FULL redirect URL (or `/callback?code=..&state=..` from a loopback listener).
    pub async fn complete_login(&self, callback_url: &str) -> Result<()> {
        self.auth.complete_login(callback_url).await
    }

    /// Android: persist this (encrypted — it holds the PKCE verifier) right after
    /// `login_url()`, so a login survives the process being killed behind the Custom Tab.
    pub fn export_pending_login(&self) -> Option<String> {
        self.auth.export_pending()
    }

    /// Android: restore the saved pending login before `complete_login` on a cold start.
    pub fn restore_pending_login(&self, saved: &str) -> Result<()> {
        self.auth.restore_pending(saved)
    }

    pub fn is_logged_in(&self) -> bool {
        self.auth.is_logged_in()
    }

    /// Clears tokens locally and stops polling; call `start_polling` again after login.
    pub fn logout(&self) -> Result<()> {
        self.stop_polling();
        self.auth.logout()
    }

    // ------------------------------------------------------------ context

    /// Set where the GM is. Narrower ids are normalized (scene → chapter → campaign).
    pub fn set_context(&self, ctx: PlayContext) -> Result<PlayContext> {
        let normalized = self.library.resolver_input(ctx)?.context;
        *self.context.lock().unwrap() = normalized;
        Ok(normalized)
    }

    pub fn context(&self) -> PlayContext {
        *self.context.lock().unwrap()
    }

    /// Resolver input for the stored context. If the current scene/chapter/campaign was
    /// deleted meanwhile, widen the context (scene → chapter → campaign → none) instead of
    /// failing every pad tap, and remember the widened context.
    fn current_input(&self) -> Result<ResolverInput> {
        let stored = self.context();
        let mut ctx = stored;
        loop {
            match self.library.resolver_input(ctx) {
                Ok(input) => {
                    if input.context != stored {
                        *self.context.lock().unwrap() = input.context;
                    }
                    return Ok(input);
                }
                Err(BardicError::NotFound(_)) if ctx != PlayContext::default() => {
                    ctx = if ctx.scene_id.is_some() {
                        PlayContext { scene_id: None, ..ctx }
                    } else if ctx.chapter_id.is_some() {
                        PlayContext { chapter_id: None, ..ctx }
                    } else {
                        PlayContext::default()
                    };
                }
                Err(e) => return Err(e),
            }
        }
    }

    // ------------------------------------------------------------ playing

    /// What would play for this trigger in the current context (no Spotify call).
    pub fn preview(&self, trigger: Trigger) -> Result<Resolution> {
        let input = self.current_input()?;
        Ok(resolver::resolve(&input, trigger))
    }

    /// GM tapped a situation and/or emotion pad: resolve and play. Returns what was chosen
    /// (`Resolution::Nothing` = nothing configured; the UI should say so).
    pub async fn trigger(&self, trigger: Trigger) -> Result<Resolution> {
        let resolution = blocking(|| self.preview(trigger))?;
        self.engine.play_resolution(&resolution).await?;
        Ok(resolution)
    }

    pub async fn play_cue(&self, cue_id: Id) -> Result<()> {
        let cue = blocking(|| self.library.get_cue(cue_id))?;
        self.engine.play_cue(&cue).await
    }

    /// Play a Spotify ambience from the ambience strip. With `emotion_id`, prefers tracks
    /// tagged with that emotion (falls back to all tracks of the environment).
    pub async fn play_environment(&self, environment_id: Id, emotion_id: Option<Id>) -> Result<()> {
        let all = blocking(|| self.library.list_ambience_tracks(Some(environment_id)))?;
        if all.is_empty() {
            return Err(BardicError::Invalid(format!(
                "environment {environment_id} has no Spotify ambience tracks"
            )));
        }
        let matching: Vec<_> =
            all.iter().filter(|t| emotion_id.is_some() && t.emotion_id == emotion_id).cloned().collect();
        let tracks = if matching.is_empty() { all } else { matching };
        self.engine.play_ambience(environment_id, &tracks, &ambience_options()).await
    }

    /// Panic/silence button.
    pub async fn silence(&self, fade_ms: u32) -> Result<()> {
        self.engine.fade_out_and_pause(fade_ms).await
    }

    pub async fn search(&self, query: &str, limit: u8) -> Result<SearchResults> {
        let token = self.auth.access_token().await?;
        match self.api.search(&token, query, limit).await {
            Err(BardicError::NotAuthenticated) => {
                self.auth.invalidate(&token);
                let token = self.auth.access_token().await?;
                self.api.search(&token, query, limit).await
            }
            other => other,
        }
    }

    // ------------------------------------------------------------ polling

    /// Start (or re-mode) background playback sync. Safe to call from any thread.
    pub fn start_polling(&self, mode: PollMode) {
        let mut poll = self.poll.lock().unwrap();
        match poll.as_ref() {
            Some(handle) => handle.set_mode(mode),
            None => {
                let _guard = self.handle.enter();
                let handle = self.engine.spawn_poll_loop();
                handle.set_mode(mode);
                *poll = Some(handle);
            }
        }
    }

    pub fn stop_polling(&self) {
        if let Some(handle) = self.poll.lock().unwrap().take() {
            handle.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::MemoryTokenStore;
    use crate::model::{CueOptions, CueOwner, PlaybackState, TrackRef};

    struct NoopObserver;
    impl EngineObserver for NoopObserver {
        fn on_playback_changed(&self, _: &PlaybackState) {}
        fn on_error(&self, _: &BardicError) {}
    }

    fn core() -> Arc<BardicCore> {
        BardicCore::new(
            CoreConfig {
                db_path: None,
                auth: AuthConfig::new("test-client", "http://127.0.0.1:43821/callback"),
                engine: EngineConfig::default(),
            },
            Arc::new(MemoryTokenStore::default()),
            Arc::new(NoopObserver),
        )
        .unwrap()
    }

    #[test]
    fn works_without_ambient_runtime_and_polls() {
        // UniFFI-style call from a plain thread: must not panic.
        let core = core();
        core.start_polling(PollMode::Idle);
        core.stop_polling();
    }

    #[test]
    fn deleted_scene_widens_context_instead_of_failing() {
        let core = core();
        let lib = core.library();
        let camp = lib.create_campaign("C", "").unwrap();
        let ch = lib.create_chapter(camp.id, "Ch", "").unwrap();
        let sc = lib.create_scene(ch.id, "S", "").unwrap();
        let combat = lib.list_tags(Some(crate::model::TagKind::Situation)).unwrap()[0].id;
        let cue = lib.create_cue(CueOwner::Chapter(ch.id), "fight", Some(combat), None, &CueOptions::default()).unwrap();
        lib.add_cue_item(
            cue.id,
            &TrackRef {
                spotify_uri: "spotify:track:x".into(),
                title: "x".into(),
                subtitle: String::new(),
                art_url: None,
                duration_ms: None,
                start_offset_ms: 0,
            },
        )
        .unwrap();
        core.set_context(PlayContext { scene_id: Some(sc.id), ..Default::default() }).unwrap();
        lib.delete_scene(sc.id).unwrap();

        let r = core.preview(Trigger { situation_id: Some(combat), emotion_id: None }).unwrap();
        assert!(matches!(r, Resolution::Cue { ref cue, .. } if cue.name == "fight"));
        assert_eq!(core.context(), PlayContext { campaign_id: Some(camp.id), chapter_id: Some(ch.id), scene_id: None });
    }
}
