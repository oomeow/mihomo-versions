//! mihomo-versions: mihomo release binary management library.
//!
//! Provides release-index parsing, platform detection, asset selection,
//! downloading, and checksum verification for mihomo kernel binaries. It is
//! consumed by applications such as Clash Verge Self; the sibling
//! `mihomo-versions-sync` binary generates the index from GitHub Releases.
//!
//! The public API is intentionally small — everything below the crate root is
//! an implementation detail and not meant for direct consumption.

// Sync-tool internals: only reachable by the `mihomo-versions-sync` binary and
// integration tests, hidden from the public docs.
#[doc(hidden)]
pub mod classify;

mod client;
mod downloader;
mod error;
mod index;
mod model;
mod platform;

pub use client::HttpClient;
pub use downloader::{
    DownloadHandle, DownloadOptions, asset_base_name, base_name, download, download_async, list_cached_downloads,
    remove_cached_download,
};
pub use error::Error;
pub use index::{
    Channel, IndexCache, VersionFilter, assets_for_platform, fetch_index, fetch_index_cached, list_versions,
    pick_asset_by_name, select_asset_by_name, sorted_versions, sorted_versions_by_channel,
};
// Used by the sync binary; not part of the consumer-facing API.
#[doc(hidden)]
pub use index::{now_rfc3339, write_atomic};
// Used by the sync binary; not part of the consumer-facing API.
#[doc(hidden)]
pub use model::CURRENT_SCHEMA_VERSION;
pub use model::{MihomoAsset, MihomoIndex, MihomoVersion, Source, normalize_tag};
pub use platform::{Platform, normalize_arch};
// 供消费者做可取消下载（与 DownloadOptions.cancel 配套）。
pub use tokio_util::sync::CancellationToken;
