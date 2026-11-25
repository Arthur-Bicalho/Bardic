//! reqwest implementation of `SpotifyApi`.
//!
//! Every request goes through [`HttpSpotify::send`], which attaches the bearer token and maps
//! non-2xx responses to [`BardicError`] in exactly one place ([`map_error`]).
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{PlayRequest, SpotifyApi};
use crate::model::{Device, PlaybackState, RepeatMode, SearchResults, TrackRef};
use crate::{BardicError, Result};

pub const DEFAULT_API_BASE: &str = "https://api.spotify.com/v1";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct HttpSpotify {
    client: reqwest::Client,
    base_url: String,
}

impl HttpSpotify {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_API_BASE)
    }

    /// For tests against wiremock: base without trailing slash, e.g. `http://127.0.0.1:1234/v1`.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("bardic-core/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self { client, base_url }
    }

    /// Send one request and return the (already status-checked) response.
    /// `body: None` on a non-GET sends an explicit `Content-Length: 0` (Spotify answers 411 otherwise).
    async fn send(
        &self,
        method: Method,
        path: &str,
        token: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, path);
        tracing::debug!(%method, %path, ?query, has_body = body.is_some(), "spotify request");

        let mut req = self.client.request(method.clone(), &url).header(AUTHORIZATION, format!("Bearer {token}"));
        if !query.is_empty() {
            req = req.query(query);
        }
        match body {
            Some(b) => req = req.header(CONTENT_TYPE, "application/json").body(b.to_string()),
            None if method != Method::GET => req = req.header(CONTENT_LENGTH, "0"),
            None => {}
        }

        let resp = req.send().await?;
        let status = resp.status();
        tracing::debug!(%path, status = status.as_u16(), "spotify response");
        if status.is_success() {
            return Ok(resp);
        }
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default();
        Err(map_error(status, path, retry_after, &text))
    }

    /// A control call whose response body is irrelevant.
    async fn command(
        &self,
        method: Method,
        path: &str,
        token: &str,
        device_id: Option<&str>,
        mut query: Vec<(&str, String)>,
        body: Option<Value>,
    ) -> Result<()> {
        if let Some(d) = device_id {
            query.push(("device_id", d.to_string()));
        }
        self.send(method, path, token, &query, body).await.map(|_| ())
    }
}

impl Default for HttpSpotify {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- error mapping

/// Pull a human message (and optional `reason`) out of Spotify's error payloads:
/// `{"error":{"status":404,"message":"...","reason":"NO_ACTIVE_DEVICE"}}` (regular API) or
/// `{"error":"invalid_token","error_description":"..."}` (auth-style).
fn parse_error_body(text: &str) -> (String, Option<String>) {
    let fallback = || (text.trim().chars().take(300).collect::<String>(), None);
    let Ok(v) = serde_json::from_str::<Value>(text) else { return fallback() };
    match v.get("error") {
        Some(Value::Object(o)) => {
            let msg = o.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
            let reason = o.get("reason").and_then(Value::as_str).map(str::to_string);
            (msg, reason)
        }
        Some(Value::String(s)) => {
            let desc = v.get("error_description").and_then(Value::as_str);
            (desc.map(|d| format!("{s}: {d}")).unwrap_or_else(|| s.clone()), None)
        }
        _ => fallback(),
    }
}

/// The single place where HTTP failures become `BardicError`s.
fn map_error(status: StatusCode, path: &str, retry_after: Option<u64>, body: &str) -> BardicError {
    let (message, reason) = parse_error_body(body);
    match status.as_u16() {
        401 => BardicError::NotAuthenticated,
        403 => BardicError::Forbidden(if message.is_empty() { "forbidden".into() } else { message }),
        404 => {
            let mentions_device = reason.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("NO_ACTIVE_DEVICE"))
                || message.to_ascii_lowercase().contains("device");
            if mentions_device || path.starts_with("/me/player") {
                BardicError::NoActiveDevice
            } else {
                BardicError::Api { status: 404, message }
            }
        }
        429 => BardicError::RateLimited { retry_after_secs: retry_after.unwrap_or(1) },
        s => BardicError::Api {
            status: s,
            message: if message.is_empty() { status.canonical_reason().unwrap_or("error").to_string() } else { message },
        },
    }
}

// ---------------------------------------------------------------- wire types (all lenient)

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireImage {
    url: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

/// Largest image by pixel area; images without dimensions rank lowest (but still usable).
fn largest_image(images: &[WireImage]) -> Option<String> {
    images
        .iter()
        .filter(|i| i.url.is_some())
        .max_by_key(|i| u64::from(i.width.unwrap_or(0)) * u64::from(i.height.unwrap_or(0)))
        .and_then(|i| i.url.clone())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireDevice {
    id: Option<String>,
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    is_active: bool,
    is_restricted: bool,
    volume_percent: Option<u32>,
    supports_volume: Option<bool>,
}

impl WireDevice {
    fn into_device(self, id: String) -> Device {
        Device {
            id,
            name: self.name.unwrap_or_default(),
            device_type: self.kind.unwrap_or_default(),
            is_active: self.is_active,
            is_restricted: self.is_restricted,
            volume_percent: self.volume_percent.map(|v| v.min(100) as u8),
            supports_volume: self.supports_volume.unwrap_or(true),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireDevices {
    devices: Vec<Option<WireDevice>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireNamed {
    name: Option<String>,
    images: Vec<WireImage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireItem {
    uri: Option<String>,
    name: Option<String>,
    duration_ms: Option<u64>,
    artists: Vec<WireNamed>,
    /// Episodes: the show.
    show: Option<WireNamed>,
    /// Tracks: the album.
    album: Option<WireNamed>,
    /// Albums (search) and episodes carry their own images.
    images: Vec<WireImage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireContext {
    uri: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WirePlayback {
    is_playing: bool,
    device: Option<WireDevice>,
    item: Option<WireItem>,
    context: Option<WireContext>,
    progress_ms: Option<u64>,
    repeat_state: Option<String>,
    shuffle_state: bool,
}

impl WirePlayback {
    fn into_state(self) -> PlaybackState {
        let device = self.device.map(|d| {
            let id = d.id.clone().unwrap_or_default();
            d.into_device(id)
        });
        let mut st = PlaybackState {
            is_playing: self.is_playing,
            device,
            context_uri: self.context.and_then(|c| c.uri),
            progress_ms: clamp_u32(self.progress_ms.unwrap_or(0)),
            repeat: self.repeat_state.as_deref().map(RepeatMode::from_spotify).unwrap_or_default(),
            shuffle: self.shuffle_state,
            ..PlaybackState::default()
        };
        if let Some(item) = self.item {
            st.track_uri = item.uri;
            st.track_name = item.name;
            st.duration_ms = clamp_u32(item.duration_ms.unwrap_or(0));
            st.artist_name = item
                .artists
                .iter()
                .find_map(|a| a.name.clone())
                .or_else(|| item.show.as_ref().and_then(|s| s.name.clone()));
            st.album_art_url = item
                .album
                .as_ref()
                .and_then(|a| largest_image(&a.images))
                .or_else(|| largest_image(&item.images))
                .or_else(|| item.show.as_ref().and_then(|s| largest_image(&s.images)));
        }
        st
    }
}

fn clamp_u32(v: u64) -> u32 {
    v.min(u64::from(u32::MAX)) as u32
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WirePage {
    items: Vec<Option<WireItem>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireOwner {
    display_name: Option<String>,
    id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WirePlaylist {
    uri: Option<String>,
    name: Option<String>,
    owner: Option<WireOwner>,
    images: Vec<WireImage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WirePlaylistPage {
    items: Vec<Option<WirePlaylist>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireSearch {
    tracks: Option<WirePage>,
    albums: Option<WirePage>,
    playlists: Option<WirePlaylistPage>,
}

fn artists_joined(artists: &[WireNamed]) -> String {
    artists.iter().filter_map(|a| a.name.as_deref()).collect::<Vec<_>>().join(", ")
}

fn track_from_item(item: WireItem) -> Option<TrackRef> {
    Some(TrackRef {
        subtitle: artists_joined(&item.artists),
        art_url: item.album.as_ref().and_then(|a| largest_image(&a.images)).or_else(|| largest_image(&item.images)),
        duration_ms: item.duration_ms.map(clamp_u32),
        spotify_uri: item.uri?,
        title: item.name.unwrap_or_default(),
        start_offset_ms: 0,
    })
}

fn album_from_item(item: WireItem) -> Option<TrackRef> {
    Some(TrackRef {
        subtitle: artists_joined(&item.artists),
        art_url: largest_image(&item.images),
        spotify_uri: item.uri?,
        title: item.name.unwrap_or_default(),
        duration_ms: None,
        start_offset_ms: 0,
    })
}

fn playlist_from_wire(p: WirePlaylist) -> Option<TrackRef> {
    let owner = p.owner.unwrap_or_default();
    Some(TrackRef {
        spotify_uri: p.uri?,
        title: p.name.unwrap_or_default(),
        subtitle: owner.display_name.or(owner.id).unwrap_or_default(),
        art_url: p.images.into_iter().find_map(|i| i.url),
        duration_ms: None,
        start_offset_ms: 0,
    })
}

// ---------------------------------------------------------------- trait impl

#[async_trait]
impl SpotifyApi for HttpSpotify {
    async fn playback_state(&self, token: &str) -> Result<Option<PlaybackState>> {
        let q = [("additional_types", "track,episode".to_string())];
        let resp = self.send(Method::GET, "/me/player", token, &q, None).await?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let text = resp.text().await?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let wire: WirePlayback = serde_json::from_str(&text)?;
        Ok(Some(wire.into_state()))
    }

    async fn devices(&self, token: &str) -> Result<Vec<Device>> {
        let resp = self.send(Method::GET, "/me/player/devices", token, &[], None).await?;
        let text = resp.text().await?;
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let wire: WireDevices = serde_json::from_str(&text)?;
        Ok(wire
            .devices
            .into_iter()
            .flatten()
            .filter_map(|d| {
                let id = d.id.clone()?;
                Some(d.into_device(id))
            })
            .collect())
    }

    async fn play(&self, token: &str, device_id: Option<&str>, req: &PlayRequest) -> Result<()> {
        let body = match req {
            PlayRequest::Uris { uris, offset_index, position_ms } => {
                json!({ "uris": uris, "offset": { "position": offset_index }, "position_ms": position_ms })
            }
            PlayRequest::Context { context_uri, position_ms } => {
                json!({ "context_uri": context_uri, "position_ms": position_ms })
            }
        };
        self.command(Method::PUT, "/me/player/play", token, device_id, vec![], Some(body)).await
    }

    async fn resume(&self, token: &str, device_id: Option<&str>) -> Result<()> {
        self.command(Method::PUT, "/me/player/play", token, device_id, vec![], None).await
    }

    async fn pause(&self, token: &str, device_id: Option<&str>) -> Result<()> {
        self.command(Method::PUT, "/me/player/pause", token, device_id, vec![], None).await
    }

    async fn next(&self, token: &str, device_id: Option<&str>) -> Result<()> {
        self.command(Method::POST, "/me/player/next", token, device_id, vec![], None).await
    }

    async fn previous(&self, token: &str, device_id: Option<&str>) -> Result<()> {
        self.command(Method::POST, "/me/player/previous", token, device_id, vec![], None).await
    }

    async fn seek(&self, token: &str, device_id: Option<&str>, position_ms: u32) -> Result<()> {
        let q = vec![("position_ms", position_ms.to_string())];
        self.command(Method::PUT, "/me/player/seek", token, device_id, q, None).await
    }

    async fn set_repeat(&self, token: &str, device_id: Option<&str>, mode: RepeatMode) -> Result<()> {
        let q = vec![("state", mode.as_spotify().to_string())];
        self.command(Method::PUT, "/me/player/repeat", token, device_id, q, None).await
    }

    async fn set_shuffle(&self, token: &str, device_id: Option<&str>, on: bool) -> Result<()> {
        let q = vec![("state", on.to_string())];
        self.command(Method::PUT, "/me/player/shuffle", token, device_id, q, None).await
    }

    async fn set_volume(&self, token: &str, device_id: Option<&str>, percent: u8) -> Result<()> {
        let q = vec![("volume_percent", percent.min(100).to_string())];
        self.command(Method::PUT, "/me/player/volume", token, device_id, q, None).await
    }

    async fn transfer(&self, token: &str, device_id: &str, play: bool) -> Result<()> {
        let body = json!({ "device_ids": [device_id], "play": play });
        self.command(Method::PUT, "/me/player", token, None, vec![], Some(body)).await
    }

    async fn search(&self, token: &str, query: &str, limit: u8) -> Result<SearchResults> {
        let q = [
            ("q", query.to_string()),
            ("type", "track,playlist,album".to_string()),
            ("limit", limit.clamp(1, 50).to_string()),
        ];
        let resp = self.send(Method::GET, "/search", token, &q, None).await?;
        let wire: WireSearch = serde_json::from_str(&resp.text().await?)?;
        Ok(SearchResults {
            tracks: wire.tracks.unwrap_or_default().items.into_iter().flatten().filter_map(track_from_item).collect(),
            playlists: wire
                .playlists
                .unwrap_or_default()
                .items
                .into_iter()
                .flatten()
                .filter_map(playlist_from_wire)
                .collect(),
            albums: wire.albums.unwrap_or_default().items.into_iter().flatten().filter_map(album_from_item).collect(),
        })
    }
}
