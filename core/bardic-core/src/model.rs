//! Domain model shared by every module and (later) exported over FFI.
//! IDs are SQLite rowids (`i64`) so they cross UniFFI / C-ABI trivially.
use serde::{Deserialize, Serialize};

pub type Id = i64;

// ---------------------------------------------------------------- hierarchy

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Campaign {
    pub id: Id,
    pub name: String,
    pub description: String,
    pub default_environment_id: Option<Id>,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chapter {
    pub id: Id,
    pub campaign_id: Id,
    pub name: String,
    pub description: String,
    pub default_environment_id: Option<Id>,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scene {
    pub id: Id,
    pub chapter_id: Id,
    pub name: String,
    pub description: String,
    pub default_environment_id: Option<Id>,
    pub position: i64,
}

// ---------------------------------------------------------------- tags

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagKind {
    Situation,
    Emotion,
}

impl TagKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TagKind::Situation => "situation",
            TagKind::Emotion => "emotion",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "situation" => Some(TagKind::Situation),
            "emotion" => Some(TagKind::Emotion),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: Id,
    pub kind: TagKind,
    pub name: String,
    /// `#RRGGBB`
    pub color: String,
    /// Icon key understood by the UIs (e.g. "swords", "beer").
    pub icon: String,
    pub position: i64,
}

// ---------------------------------------------------------------- cues

/// Where a cue lives in the hierarchy. Resolution walks Scene → Chapter → Campaign → Global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "level", content = "id", rename_all = "snake_case")]
pub enum CueOwner {
    Global,
    Campaign(Id),
    Chapter(Id),
    Scene(Id),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    #[default]
    Off,
    /// Loop the current track (`repeat?state=track`).
    Track,
    /// Loop the whole list/context (`repeat?state=context`).
    Context,
}

impl RepeatMode {
    pub fn as_spotify(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::Track => "track",
            RepeatMode::Context => "context",
        }
    }
    pub fn from_spotify(s: &str) -> Self {
        match s {
            "track" => RepeatMode::Track,
            "context" => RepeatMode::Context,
            _ => RepeatMode::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueOptions {
    pub repeat: RepeatMode,
    pub shuffle: bool,
    /// Target Spotify volume 0..=100; `None` = leave volume unchanged.
    pub volume: Option<u8>,
    pub fade_in_ms: u32,
    pub fade_out_ms: u32,
}

impl Default for CueOptions {
    fn default() -> Self {
        Self { repeat: RepeatMode::Track, shuffle: false, volume: None, fade_in_ms: 1500, fade_out_ms: 1500 }
    }
}

/// A Spotify reference (track, album or playlist) with cached display metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRef {
    /// `spotify:track:…`, `spotify:album:…` or `spotify:playlist:…`
    pub spotify_uri: String,
    pub title: String,
    pub subtitle: String,
    pub art_url: Option<String>,
    pub duration_ms: Option<u32>,
    /// Skip intros: start playback at this offset (tracks only).
    pub start_offset_ms: u32,
}

impl TrackRef {
    pub fn is_track(&self) -> bool {
        self.spotify_uri.starts_with("spotify:track:")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueItem {
    pub id: Id,
    pub cue_id: Id,
    pub track: TrackRef,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    pub id: Id,
    pub owner: CueOwner,
    pub name: String,
    pub situation_id: Option<Id>,
    pub emotion_id: Option<Id>,
    pub options: CueOptions,
    pub position: i64,
    /// Ordered by `position`.
    pub items: Vec<CueItem>,
}

// ---------------------------------------------------------------- ambience

/// A place/mood bucket of ambient music: Forest, Cave, Tavern, Storm…
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub id: Id,
    pub name: String,
    pub icon: String,
    pub color: String,
    pub position: i64,
}

/// Spotify ambience (replaces the music when played).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmbienceTrack {
    pub id: Id,
    pub environment_id: Id,
    pub track: TrackRef,
    /// Optional emotion so "Forest + Mystery" can pick a fitting entry.
    pub emotion_id: Option<Id>,
    pub position: i64,
}

/// Local audio loop played by Bardic itself *under* the Spotify music.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmbienceLayer {
    pub id: Id,
    pub environment_id: Id,
    pub name: String,
    /// Absolute path to an ogg/mp3/flac/wav file in app storage.
    pub file_path: String,
    /// 0.0..=1.0
    pub default_volume: f32,
    pub position: i64,
}

// ---------------------------------------------------------------- resolution

/// Where the GM currently is. `chapter_id`/`scene_id` narrow the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlayContext {
    pub campaign_id: Option<Id>,
    pub chapter_id: Option<Id>,
    pub scene_id: Option<Id>,
}

/// What the GM tapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Trigger {
    pub situation_id: Option<Id>,
    pub emotion_id: Option<Id>,
}

/// Everything the pure resolver needs; built by `db::Library::resolver_input`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolverInput {
    pub context: PlayContext,
    /// Default environment at each level (None = not set / level absent).
    pub scene_default_env: Option<Id>,
    pub chapter_default_env: Option<Id>,
    pub campaign_default_env: Option<Id>,
    pub global_default_env: Option<Id>,
    /// All cues owned by the levels of `context` plus Global cues.
    pub cues: Vec<Cue>,
    /// All Spotify ambience tracks, every environment.
    pub ambience: Vec<AmbienceTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Cue has exactly the requested situation and emotion.
    Exact,
    /// Request had both tags; cue matches the situation and has no emotion.
    SituationOnly,
    /// Request had both tags; cue matches the emotion and has no situation.
    EmotionOnly,
    /// Request had ONE tag; cue has that tag plus another one
    /// (e.g. "Combat" pad plays a "Combat + Tension" cue when no plain Combat cue exists).
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackLevel {
    Scene,
    Chapter,
    Campaign,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    Cue { cue: Cue, matched: MatchKind },
    Ambience { environment_id: Id, from: FallbackLevel, tracks: Vec<AmbienceTrack>, options: CueOptions },
    Nothing,
}

// ---------------------------------------------------------------- playback

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// "Computer", "Smartphone", "Tablet", "Speaker", …
    pub device_type: String,
    pub is_active: bool,
    pub is_restricted: bool,
    pub volume_percent: Option<u8>,
    pub supports_volume: bool,
}

/// Snapshot of Spotify's player as last known by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub device: Option<Device>,
    pub track_uri: Option<String>,
    pub track_name: Option<String>,
    pub artist_name: Option<String>,
    pub album_art_url: Option<String>,
    pub context_uri: Option<String>,
    pub progress_ms: u32,
    pub duration_ms: u32,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    /// Wall-clock ms when this snapshot was taken (for local progress interpolation).
    pub fetched_at_ms: u64,
    /// The Bardic cue/ambience that started the current playback, if any.
    pub active_cue_id: Option<Id>,
    pub active_environment_id: Option<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResults {
    pub tracks: Vec<TrackRef>,
    pub playlists: Vec<TrackRef>,
    pub albums: Vec<TrackRef>,
}
