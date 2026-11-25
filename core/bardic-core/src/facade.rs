//! `BardicCore`: the single object UI layers talk to (Tauri commands now, UniFFI later).
//! It wires the library (SQLite), login (`TokenManager`), the Spotify client and the
//! playback `Engine`, and remembers where the GM currently is (`PlayContext`).
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::auth::{AccessTokenProvider, AuthConfig, TokenManager, TokenStore};
use crate::clock::{Clock, SystemClock};
use crate::db::Library;
use crate::engine::{Engine, EngineConfig, EngineObserver, PollHandle, PollMode};
use crate::model::{Id, PlayContext, Resolution, SearchResults, Trigger};
use crate::resolver::{self, ambience_options};
use crate::spotify::{HttpSpotify, SpotifyApi};
use crate::{BardicError, Result};

pub struct CoreConfig {
    /// SQLite file; `None` = in-memory (tests / demo).
    pub db_path: Option<PathBuf>,
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
        let library = Arc::new(match &config.db_path {
            Some(p) => {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| BardicError::Db(e.to_string()))?;
                }
                Library::open(p)?
            }
            None => Library::open_in_memory()?,
        });
        let auth = Arc::new(TokenManager::new(config.auth, token_store, clock.clone()));
        let tokens: Arc<dyn AccessTokenProvider> = auth.clone();
        let engine = Engine::new(api.clone(), tokens, clock, observer, config.engine);
        Ok(Arc::new(Self {
            library,
            auth,
            api,
            engine,
            context: Mutex::new(PlayContext::default()),
            poll: Mutex::new(None),
        }))
    }

    // ------------------------------------------------------------ parts

    /// Campaign/chapter/scene/cue/tag/ambience CRUD.
    pub fn library(&self) -> &Arc<Library> {
        &self.library
    }

    /// Transport controls, devices, playback state.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
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

    pub fn is_logged_in(&self) -> bool {
        self.auth.is_logged_in()
    }

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

    // ------------------------------------------------------------ playing

    /// What would play for this trigger in the current context (no Spotify call).
    pub fn preview(&self, trigger: Trigger) -> Result<Resolution> {
        let input = self.library.resolver_input(self.context())?;
        Ok(resolver::resolve(&input, trigger))
    }

    /// GM tapped a situation and/or emotion pad: resolve and play. Returns what was chosen.
    pub async fn trigger(&self, trigger: Trigger) -> Result<Resolution> {
        let resolution = self.preview(trigger)?;
        self.engine.play_resolution(&resolution).await?;
        Ok(resolution)
    }

    pub async fn play_cue(&self, cue_id: Id) -> Result<()> {
        let cue = self.library.get_cue(cue_id)?;
        self.engine.play_cue(&cue).await
    }

    /// Play a Spotify ambience from the ambience strip. With `emotion_id`, prefers tracks
    /// tagged with that emotion (falls back to all tracks of the environment).
    pub async fn play_environment(&self, environment_id: Id, emotion_id: Option<Id>) -> Result<()> {
        let all = self.library.list_ambience_tracks(Some(environment_id))?;
        if all.is_empty() {
            return Err(BardicError::Invalid(format!(
                "environment {environment_id} has no Spotify ambience tracks"
            )));
        }
        let matching: Vec<_> = all.iter().filter(|t| emotion_id.is_some() && t.emotion_id == emotion_id).cloned().collect();
        let tracks = if matching.is_empty() { all } else { matching };
        self.engine.play_ambience(environment_id, &tracks, &ambience_options()).await
    }

    /// Panic/silence button.
    pub async fn silence(&self, fade_ms: u32) -> Result<()> {
        self.engine.fade_out_and_pause(fade_ms).await
    }

    pub async fn search(&self, query: &str, limit: u8) -> Result<SearchResults> {
        let token = self.auth.access_token().await?;
        self.api.search(&token, query, limit).await
    }

    // ------------------------------------------------------------ polling

    /// Start (or re-mode) background playback sync. Requires a tokio runtime.
    pub fn start_polling(&self, mode: PollMode) {
        let mut poll = self.poll.lock().unwrap();
        match poll.as_ref() {
            Some(handle) => handle.set_mode(mode),
            None => {
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
