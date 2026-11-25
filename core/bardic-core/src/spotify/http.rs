//! reqwest implementation of `SpotifyApi`. OWNER: spotify agent.
use async_trait::async_trait;

use super::{PlayRequest, SpotifyApi};
use crate::model::{Device, PlaybackState, RepeatMode, SearchResults};
use crate::Result;

pub const DEFAULT_API_BASE: &str = "https://api.spotify.com/v1";

pub struct HttpSpotify {
    _client: reqwest::Client,
    _base_url: String,
}

impl HttpSpotify {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_API_BASE)
    }
    /// For tests against wiremock: base without trailing slash, e.g. `http://127.0.0.1:1234/v1`.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self { _client: reqwest::Client::new(), _base_url: base_url.into() }
    }
}

impl Default for HttpSpotify {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SpotifyApi for HttpSpotify {
    async fn playback_state(&self, _token: &str) -> Result<Option<PlaybackState>> { todo!() }
    async fn devices(&self, _token: &str) -> Result<Vec<Device>> { todo!() }
    async fn play(&self, _token: &str, _device_id: Option<&str>, _req: &PlayRequest) -> Result<()> { todo!() }
    async fn resume(&self, _token: &str, _device_id: Option<&str>) -> Result<()> { todo!() }
    async fn pause(&self, _token: &str, _device_id: Option<&str>) -> Result<()> { todo!() }
    async fn next(&self, _token: &str, _device_id: Option<&str>) -> Result<()> { todo!() }
    async fn previous(&self, _token: &str, _device_id: Option<&str>) -> Result<()> { todo!() }
    async fn seek(&self, _token: &str, _device_id: Option<&str>, _position_ms: u32) -> Result<()> { todo!() }
    async fn set_repeat(&self, _token: &str, _device_id: Option<&str>, _mode: RepeatMode) -> Result<()> { todo!() }
    async fn set_shuffle(&self, _token: &str, _device_id: Option<&str>, _on: bool) -> Result<()> { todo!() }
    async fn set_volume(&self, _token: &str, _device_id: Option<&str>, _percent: u8) -> Result<()> { todo!() }
    async fn transfer(&self, _token: &str, _device_id: &str, _play: bool) -> Result<()> { todo!() }
    async fn search(&self, _token: &str, _query: &str, _limit: u8) -> Result<SearchResults> { todo!() }
}
