//! Spotify Web API surface used by Bardic.
//!
//! `SpotifyApi` is the contract the engine depends on (mocked in tests).
//! `HttpSpotify` is the reqwest implementation (owned by the spotify agent).
use async_trait::async_trait;

use crate::model::{Device, PlaybackState, RepeatMode, SearchResults};
use crate::Result;

/// What to start playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayRequest {
    /// Explicit list of track URIs (`{"uris": [...]}`), starting at `offset_index`.
    Uris { uris: Vec<String>, offset_index: u32, position_ms: u32 },
    /// Album/playlist context (`{"context_uri": ...}`).
    Context { context_uri: String, position_ms: u32 },
}

/// All calls take an already-valid bearer token; token refresh is the caller's job
/// (see `auth::TokenManager`). `device_id: None` targets the currently active device.
#[async_trait]
pub trait SpotifyApi: Send + Sync {
    /// `GET /v1/me/player` — `Ok(None)` on 204 (nothing playing / no device).
    /// `active_cue_id`/`active_environment_id` are left `None`; `fetched_at_ms` = 0 (engine fills it).
    async fn playback_state(&self, token: &str) -> Result<Option<PlaybackState>>;
    /// `GET /v1/me/player/devices`
    async fn devices(&self, token: &str) -> Result<Vec<Device>>;
    /// `PUT /v1/me/player/play`
    async fn play(&self, token: &str, device_id: Option<&str>, req: &PlayRequest) -> Result<()>;
    /// `PUT /v1/me/player/play` with no body (resume).
    async fn resume(&self, token: &str, device_id: Option<&str>) -> Result<()>;
    /// `PUT /v1/me/player/pause`
    async fn pause(&self, token: &str, device_id: Option<&str>) -> Result<()>;
    /// `POST /v1/me/player/next`
    async fn next(&self, token: &str, device_id: Option<&str>) -> Result<()>;
    /// `POST /v1/me/player/previous`
    async fn previous(&self, token: &str, device_id: Option<&str>) -> Result<()>;
    /// `PUT /v1/me/player/seek?position_ms=`
    async fn seek(&self, token: &str, device_id: Option<&str>, position_ms: u32) -> Result<()>;
    /// `PUT /v1/me/player/repeat?state=track|context|off`
    async fn set_repeat(&self, token: &str, device_id: Option<&str>, mode: RepeatMode) -> Result<()>;
    /// `PUT /v1/me/player/shuffle?state=`
    async fn set_shuffle(&self, token: &str, device_id: Option<&str>, on: bool) -> Result<()>;
    /// `PUT /v1/me/player/volume?volume_percent=`
    async fn set_volume(&self, token: &str, device_id: Option<&str>, percent: u8) -> Result<()>;
    /// `PUT /v1/me/player` `{"device_ids":[id],"play":play}`
    async fn transfer(&self, token: &str, device_id: &str, play: bool) -> Result<()>;
    /// `GET /v1/search?type=track,playlist,album&limit=`
    async fn search(&self, token: &str, query: &str, limit: u8) -> Result<SearchResults>;
}

pub mod http;
pub use http::HttpSpotify;
