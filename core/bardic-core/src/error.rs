use thiserror::Error;

pub type Result<T, E = BardicError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
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
    /// Spotify answered 403 — usually means the account is not Premium.
    #[error("Spotify Premium required or action forbidden: {0}")]
    Forbidden(String),
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
