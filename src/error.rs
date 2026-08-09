use thiserror::Error;

/// Errors returned by this library.
///
/// The library never panics; every failure path surfaces through this enum.
#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("http error {0}")]
    Http(u16),
    #[error("download cancelled")]
    Cancelled,
    #[error("download timed out (no data received within the idle timeout)")]
    Timeout,
    #[error("GitHub API rate limit exceeded (HTTP 403)")]
    RateLimited,
    #[error("invalid GitHub token: {0}")]
    InvalidToken(String),
    #[error("failed to decode JSON: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("checksum mismatch: expected {expected}, computed {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error(
        "no asset named {name} found{version}",
        version = version.as_ref().map_or(String::new(), |v| format!(" for version {v}"))
    )]
    AssetNotFound { name: String, version: Option<String> },
    #[error(
        "asset {name} is not available (HTTP 404 at {url}); the release may have been removed — re-sync the index and retry"
    )]
    AssetUnavailable { name: String, url: String },
    #[error("invalid index schema: {0}")]
    InvalidSchema(String),
    #[error("archive contains no usable binary: {0}")]
    ArchiveContent(String),
    #[error("zip archive error: {0}")]
    Zip(#[from] zip::result::ZipError),
}
