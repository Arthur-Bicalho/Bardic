use serde::Serialize;
use thiserror::Error;

pub type Result<T, E = BardicError> = std::result::Result<T, E>;

/// Serializes as `{"kind": "no_active_device", "detail": ...}` for Tauri / UI layers.
#[derive(Debug, Error, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum BardicError {
    #[error("database error: {0}")]
    Db(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("token storage error: {0}")]
    TokenStore(String),
    /// Spotify answered 404 "no active device" — UI should open the device picker.
    #[error("no active Spotify device")]
    NoActiveDevice,
    /// Spotify answered 403 — Premium required or otherwise not allowed for this account.
    #[error("Spotify Premium required or action forbidden: {0}")]
    Forbidden(String),
    /// Spotify answered 403 "Restriction violated": the command doesn't apply to the
    /// player's current state (e.g. pausing an already paused player). Usually harmless.
    #[error("player command not allowed right now: {0}")]
    Restricted(String),
    /// 429; caller may retry after `retry_after_secs`.
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("Spotify API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("network error: {0}")]
    Network(String),
}

impl From<rusqlite::Error> for BardicError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => BardicError::NotFound("row".into()),
            rusqlite::Error::SqliteFailure(f, _)
                if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
            {
                BardicError::Invalid("referenced item does not exist".into())
            }
            other => BardicError::Db(other.to_string()),
        }
    }
}

impl From<rusqlite_migration::Error> for BardicError {
    fn from(e: rusqlite_migration::Error) -> Self {
        BardicError::Db(e.to_string())
    }
}

impl From<reqwest::Error> for BardicError {
    fn from(e: reqwest::Error) -> Self {
        BardicError::Network(e.to_string())
    }
}

impl From<serde_json::Error> for BardicError {
    fn from(e: serde_json::Error) -> Self {
        BardicError::Invalid(format!("json: {e}"))
    }
}
