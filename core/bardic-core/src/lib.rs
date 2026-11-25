//! Bardic core: campaign/chapter/scene music library, cue resolution,
//! Spotify Web API control and playback state. UI layers (Tauri on Linux,
//! Kotlin on Android via UniFFI) talk only to this crate.
//!
//! Module ownership (see docs/design.md §5):
//! - `model`, `error`, `clock`  — shared contracts (do not change signatures casually)
//! - `db`       — SQLite schema, migrations, CRUD, seed data
//! - `resolver` — pure cue / ambience-fallback resolution
//! - `auth`     — OAuth 2.0 PKCE + token lifecycle
//! - `spotify`  — `SpotifyApi` trait + reqwest implementation
//! - `engine`   — playback state machine, poll loop, fades

pub mod auth;
pub mod clock;
pub mod db;
pub mod engine;
pub mod error;
pub mod model;
pub mod resolver;
pub mod spotify;

pub use error::{BardicError, Result};
