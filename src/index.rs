use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::StatusCode;
use semver::Version;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    client::HttpClient,
    error::Error,
    model::{CURRENT_SCHEMA_VERSION, MihomoAsset, MihomoIndex, MihomoVersion, normalize_tag},
    platform::Platform,
};

/// Fetches and parses a version index, trying each candidate index file URL in
/// order until one succeeds (multi-mirror failover). URLs ending in `.gz` are
/// gzip-decompressed before parsing. Returns the last error when every URL
/// fails.
pub async fn fetch_index(client: &HttpClient, urls: &[&str]) -> Result<MihomoIndex, Error> {
    if urls.is_empty() {
        return Err(Error::InvalidSchema("fetch_index requires at least one URL".to_string()));
    }
    let mut last_err: Option<Error> = None;
    for url in urls {
        log::debug!("fetching index from {url}");
        match fetch_index_bytes(client, url).await {
            Ok(bytes) => {
                let index: MihomoIndex = serde_json::from_slice(&bytes)?;
                validate_schema(&index);
                return Ok(index);
            }
            Err(e) => {
                log::warn!("failed to fetch index from {url}: {e}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::InvalidSchema("fetch_index requires at least one URL".to_string())))
}

/// Fetches an index file, decompressing it first when the URL ends in `.gz`.
async fn fetch_index_bytes(client: &HttpClient, url: &str) -> Result<Vec<u8>, Error> {
    let bytes = client.get_bytes(url).await?;
    maybe_gunzip(url, &bytes)
}

/// Decompresses a gzip payload for `.gz` URLs; other payloads pass through.
fn maybe_gunzip(url: &str, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if url.ends_with(".gz") {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).map_err(Error::Io)?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// Local cache for the version index.
#[derive(Clone)]
pub struct IndexCache {
    /// Path of the cached index JSON file.
    pub path: PathBuf,
    /// How long a cached index is considered fresh before a revalidation.
    pub max_age: Duration,
}

impl IndexCache {
    fn fresh(&self, meta: &Option<IndexMeta>) -> bool {
        // The cache file itself must exist: a fresh meta alone (e.g. left over
        // after the index file was deleted) must not be served.
        self.path.is_file()
            && meta
                .as_ref()
                .and_then(|m| m.fetched_at())
                .is_some_and(|at| OffsetDateTime::now_utc() - at <= self.max_age)
    }

    /// Deletes the cached index file and its metadata sidecar. Idempotent:
    /// succeeds even when the cache files do not exist.
    pub async fn clear(&self) -> Result<(), Error> {
        for path in [&self.path, &cache_meta_path(&self.path)] {
            match tokio::fs::remove_file(path).await {
                Ok(()) => log::debug!("deleted index cache {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// Fetches a version index with local caching and multi-mirror failover.
///
/// - a fresh cache (`age <= max_age`) is served without any network request;
/// - a stale cache triggers a conditional request (`If-None-Match` /
///   `If-Modified-Since`): 304 keeps the cache, 200 refreshes it;
/// - when every mirror fails, an existing cache is served stale (with a warning)
///   instead of erroring.
pub async fn fetch_index_cached(client: &HttpClient, urls: &[&str], cache: &IndexCache) -> Result<MihomoIndex, Error> {
    if urls.is_empty() {
        return Err(Error::InvalidSchema("fetch_index requires at least one URL".to_string()));
    }
    let meta_path = cache_meta_path(&cache.path);
    // The meta is only meaningful when the cache file exists: without the file
    // there is nothing to revalidate or serve, so fetch fresh (no conditional
    // headers) and let the 200 path create the cache file itself.
    let meta: Option<IndexMeta> = if cache.path.is_file() { load_meta(&meta_path).await } else { None };

    if cache.fresh(&meta) {
        log::debug!("index cache fresh; serving {} without network", cache.path.display());
        return parse_index_file(&cache.path).await;
    }

    let mut last_err: Option<Error> = None;
    for url in urls {
        match client
            .open_index(
                url,
                meta.as_ref().and_then(|m| m.etag.as_deref()),
                meta.as_ref().and_then(|m| m.last_modified.as_deref()),
            )
            .await
        {
            Ok(response) => match response.status() {
                StatusCode::OK => {
                    return store_index(cache, &meta_path, url, response).await;
                }
                StatusCode::NOT_MODIFIED => {
                    // A 304 means "use your copy"; if the file vanished in the
                    // meantime, refetch without conditional headers so the
                    // cache file is (re)created instead of erroring.
                    if !cache.path.is_file() {
                        log::warn!(
                            "index 304 for {url} but cache file missing; refetching without conditional headers"
                        );
                        let response = client.open_index(url, None, None).await?;
                        if response.status() == StatusCode::OK {
                            return store_index(cache, &meta_path, url, response).await;
                        }
                        last_err = Some(crate::client::status_to_error(response.status()));
                        continue;
                    }
                    log::debug!("index not modified (304); using cached copy");
                    let mut meta =
                        meta.unwrap_or(IndexMeta { etag: None, last_modified: None, fetched_at: now_rfc3339() });
                    meta.fetched_at = now_rfc3339();
                    write_meta(&meta_path, &meta).await?;
                    return parse_index_file(&cache.path).await;
                }
                other => {
                    log::warn!("index mirror {url} returned {other}");
                    last_err = Some(crate::client::status_to_error(other));
                }
            },
            Err(e) => {
                log::warn!("failed to fetch index from {url}: {e}");
                last_err = Some(e);
            }
        }
    }

    // Stale fallback only makes sense when the cache file actually exists (a
    // leftover meta without the index file is not servable).
    if cache.path.is_file() {
        log::warn!("all index mirrors failed; falling back to stale cache {}", cache.path.display());
        return parse_index_file(&cache.path).await;
    }
    Err(last_err.unwrap_or_else(|| Error::InvalidSchema("fetch_index requires at least one URL".to_string())))
}

/// Parses a 200 response body (gzip-decompressing `.gz` URLs first), stores the
/// plain-JSON index in the cache (creating the cache file if needed), and
/// returns the parsed index.
async fn store_index(
    cache: &IndexCache,
    meta_path: &Path,
    url: &str,
    response: reqwest::Response,
) -> Result<MihomoIndex, Error> {
    let meta = IndexMeta::from_response(&response);
    let bytes = response.bytes().await?;
    let bytes = maybe_gunzip(url, &bytes)?;
    let index: MihomoIndex = serde_json::from_slice(&bytes)?;
    validate_schema(&index);
    save_cache(&cache.path, meta_path, &bytes, meta).await?;
    Ok(index)
}

/// Returns versions sorted newest-first: non-semver tags on top (tie-broken by
/// `published_at` descending), then semver descending.
pub fn sorted_versions(index: &MihomoIndex) -> Vec<&MihomoVersion> {
    let mut versions: Vec<&MihomoVersion> = index.versions.iter().collect();
    versions.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
    versions
}

/// Distribution channel of a version, as classified at sync time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Alpha,
    Nightly,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Alpha => "alpha",
            Channel::Nightly => "nightly",
        }
    }

    /// Parses a channel name, returning `None` for unknown values.
    pub fn parse(s: &str) -> Option<Channel> {
        match s {
            "stable" => Some(Channel::Stable),
            "alpha" => Some(Channel::Alpha),
            "nightly" => Some(Channel::Nightly),
            _ => None,
        }
    }
}

/// Versions of a given channel, sorted newest-first (see [`sorted_versions`]).
pub fn sorted_versions_by_channel(index: &MihomoIndex, channel: Channel) -> Vec<&MihomoVersion> {
    sorted_versions(index).into_iter().filter(|v| v.channel == channel.as_str()).collect()
}

/// Filters for [`list_versions`] / [`assets_for_platform`].
#[derive(Debug, Clone, Default)]
pub struct VersionFilter {
    pub channel: Option<Channel>,
    pub prerelease: Option<bool>,
    /// Case-insensitive substring match against the tag or semver.
    pub search: Option<String>,
}

impl VersionFilter {
    fn matches(&self, version: &MihomoVersion) -> bool {
        if self.channel.is_some_and(|c| version.channel != c.as_str()) {
            return false;
        }
        if self.prerelease.is_some_and(|p| version.prerelease != p) {
            return false;
        }
        match &self.search {
            Some(needle) => {
                // Pre-lowercase the needle once; the haystack comparison is
                // ASCII-case-insensitive without allocating a lowered copy.
                let needle = needle.to_ascii_lowercase();
                contains_ignore_ascii_case(&version.tag, &needle)
                    || version.semver.as_deref().is_some_and(|s| contains_ignore_ascii_case(s, &needle))
            }
            None => true,
        }
    }
}

/// ASCII-case-insensitive substring check, allocation-free.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    needle.is_empty() || haystack.as_bytes().windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Versions matching `filter` (or all, when `None`), sorted newest-first.
pub fn list_versions<'a>(index: &'a MihomoIndex, filter: Option<&VersionFilter>) -> Vec<&'a MihomoVersion> {
    sorted_versions(index).into_iter().filter(|v| filter.is_none_or(|f| f.matches(v))).collect()
}

/// Versions carrying assets of `platform`, filtered by `filter` (or all, when
/// `None`), newest-first. Each returned version's `assets` field holds **all**
/// of that version's assets for the platform (the version's other-platform
/// assets are dropped). Versions without assets for the platform are omitted.
/// Returns owned values, not references.
pub fn assets_for_platform(
    index: &MihomoIndex,
    platform: Platform,
    filter: Option<&VersionFilter>,
) -> Vec<MihomoVersion> {
    list_versions(index, filter)
        .into_iter()
        .filter_map(|version| {
            let assets: Vec<MihomoAsset> =
                version.assets.iter().filter(|asset| asset.platform == platform.as_str()).cloned().collect();
            if assets.is_empty() {
                return None;
            }
            let mut version = version.clone();
            version.assets = assets;
            Some(version)
        })
        .collect()
}

/// Picks the asset whose name exactly matches `asset_name` within `version`.
pub fn select_asset_by_name<'a>(version: &'a MihomoVersion, asset_name: &str) -> Result<&'a MihomoAsset, Error> {
    version
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| Error::AssetNotFound { name: asset_name.to_string(), version: Some(version.tag.clone()) })
}

/// Resolves an asset by exact name, optionally constrained to a specific
/// version. With no version, searches the newest versions first and returns
/// the first match (the asset name usually embeds the version, so this picks
/// the newest build carrying that name).
pub fn pick_asset_by_name<'a>(
    index: &'a MihomoIndex,
    asset_name: &str,
    version: Option<&str>,
) -> Result<&'a MihomoAsset, Error> {
    match version {
        Some(v) => {
            let normalized = normalize_tag(v);
            let target = index
                .versions
                .iter()
                .find(|ver| {
                    ver.tag == v
                        || ver.semver.as_deref() == Some(v)
                        || (normalized.is_some() && ver.semver.as_deref() == normalized.as_deref())
                })
                .ok_or_else(|| Error::AssetNotFound { name: asset_name.to_string(), version: Some(v.to_string()) })?;
            select_asset_by_name(target, asset_name)
        }
        None => {
            for ver in sorted_versions(index) {
                if let Ok(asset) = select_asset_by_name(ver, asset_name) {
                    return Ok(asset);
                }
            }
            Err(Error::AssetNotFound { name: asset_name.to_string(), version: None })
        }
    }
}

/// Sort key for [`sorted_versions`]: (semver-or-not, semver, `published_at`).
/// Non-semver tags (latest builds) sort on top; among equal semvers the later
/// `published_at` wins. `published_at` is borrowed — comparing `&str` orders
/// exactly like the owned `String`, without allocating per version per sort.
type VersionKey<'a> = (u8, Version, &'a str);

fn version_key(v: &MihomoVersion) -> VersionKey<'_> {
    let published: &str = v.published_at.as_deref().unwrap_or_default();
    match v.semver.as_deref().and_then(|s| Version::parse(s).ok()) {
        Some(sv) => (0, sv, published),
        // Non-semver tags (latest builds like `Prerelease-Alpha`) sort on top.
        None => (1, Version::new(0, 0, 0), published),
    }
}

fn validate_schema(index: &MihomoIndex) {
    log::debug!("index: schema_version={} versions={}", index.schema_version, index.versions.len());
    if index.schema_version > CURRENT_SCHEMA_VERSION {
        log::warn!(
            "index schema_version {} is newer than supported {CURRENT_SCHEMA_VERSION}; parsing best-effort",
            index.schema_version
        );
    }
}

// --- cache plumbing ---------------------------------------------------------

/// Sidecar metadata for a cached index. `fetched_at` is stored as an RFC3339
/// string so the file stays human-readable and parseable without time-crate
/// serde quirks.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
    fetched_at: String,
}

impl IndexMeta {
    fn from_response(response: &reqwest::Response) -> Self {
        Self {
            etag: response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string),
            last_modified: response
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            fetched_at: now_rfc3339(),
        }
    }

    fn fetched_at(&self) -> Option<OffsetDateTime> {
        OffsetDateTime::parse(&self.fetched_at, &Rfc3339).ok()
    }
}

/// Current UTC time as an RFC3339 string. Shared with the sync binary; not
/// part of the consumer-facing API.
#[doc(hidden)]
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn cache_meta_path(path: &Path) -> PathBuf {
    path.with_extension("json.meta")
}

async fn load_meta(path: &Path) -> Option<IndexMeta> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_meta(path: &Path, meta: &IndexMeta) -> Result<(), Error> {
    let bytes = serde_json::to_vec(meta)?;
    write_atomic(path, &bytes)?;
    Ok(())
}

async fn save_cache(path: &Path, meta_path: &Path, bytes: &[u8], meta: IndexMeta) -> Result<(), Error> {
    write_atomic(path, bytes)?;
    write_meta(meta_path, &meta).await
}

/// Process-unique sequence for atomic-write temp names: the pid alone would
/// collide when two tasks in the same process write the same path concurrently.
static TMP_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Atomic file write: tmp file in the same directory, fsynced before the
/// rename so a crash cannot leave a zero-length file at the target path.
/// Shared with the sync binary; not part of the consumer-facing API.
#[doc(hidden)]
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

async fn parse_index_file(path: &Path) -> Result<MihomoIndex, Error> {
    let text = tokio::fs::read_to_string(path).await?;
    let index: MihomoIndex = serde_json::from_str(&text)?;
    validate_schema(&index);
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(etag: Option<&str>, fetched_at: &str) -> IndexMeta {
        IndexMeta { etag: etag.map(str::to_string), last_modified: None, fetched_at: fetched_at.to_string() }
    }

    #[test]
    fn cache_freshness_by_max_age() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.json");
        std::fs::write(&path, b"{}").unwrap();
        let cache = IndexCache { path, max_age: Duration::from_secs(3600) };
        assert!(cache.fresh(&Some(meta(Some("x"), &now_rfc3339()))));
        assert!(!cache.fresh(&Some(meta(Some("x"), "2020-01-01T00:00:00Z"))));
        assert!(!cache.fresh(&None));
    }

    #[test]
    fn fresh_meta_without_cache_file_is_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let cache = IndexCache { path: dir.path().join("missing.json"), max_age: Duration::from_secs(3600) };
        // The meta is fresh but the cache file itself does not exist: must not
        // be served from cache (fall back to the network).
        assert!(!cache.fresh(&Some(meta(Some("x"), &now_rfc3339()))));
    }

    #[tokio::test]
    async fn clear_removes_cache_and_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::write(cache_meta_path(&path), b"{}").unwrap();
        let cache = IndexCache { path, max_age: Duration::from_secs(3600) };
        assert!(cache.path.is_file());
        cache.clear().await.unwrap();
        assert!(!cache.path.exists());
        assert!(!cache_meta_path(&cache.path).exists());
        // Idempotent: clearing again must not error.
        cache.clear().await.unwrap();
    }

    #[test]
    fn channel_as_str_and_parse_roundtrip() {
        for c in [Channel::Stable, Channel::Alpha, Channel::Nightly] {
            assert_eq!(Channel::parse(c.as_str()), Some(c));
        }
        assert_eq!(Channel::parse("beta"), None);
    }

    #[test]
    fn sorted_versions_by_channel_filters_and_sorts() {
        fn version(tag: &str, channel: &str) -> MihomoVersion {
            MihomoVersion {
                semver: Some(tag.trim_start_matches('v').to_string()),
                tag: tag.to_string(),
                prerelease: channel != "stable",
                channel: channel.to_string(),
                published_at: Some("2026-01-01T00:00:00Z".to_string()),
                created_at: None,
                updated_at: None,
                assets: vec![],
            }
        }
        let index = MihomoIndex {
            schema_version: 1,
            source: None,
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            versions: vec![
                version("v1.0.0", "stable"),
                version("Prerelease-Alpha", "alpha"),
                version("v0.9.0", "stable"),
            ],
        };
        let stables: Vec<&str> =
            sorted_versions_by_channel(&index, Channel::Stable).iter().map(|v| v.tag.as_str()).collect();
        assert_eq!(stables, vec!["v1.0.0", "v0.9.0"]);
        let alphas: Vec<&str> =
            sorted_versions_by_channel(&index, Channel::Alpha).iter().map(|v| v.tag.as_str()).collect();
        assert_eq!(alphas, vec!["Prerelease-Alpha"]);
        assert!(sorted_versions_by_channel(&index, Channel::Nightly).is_empty());
    }

    fn version(tag: &str, channel: &str, prerelease: bool, assets: Vec<MihomoAsset>) -> MihomoVersion {
        MihomoVersion {
            semver: (tag != "Prerelease-Alpha").then(|| tag.trim_start_matches('v').to_string()),
            tag: tag.to_string(),
            prerelease,
            channel: channel.to_string(),
            published_at: Some("2026-01-01T00:00:00Z".to_string()),
            created_at: None,
            updated_at: None,
            assets,
        }
    }

    fn asset(name: &str, platform: &str) -> MihomoAsset {
        MihomoAsset {
            name: name.to_string(),
            platform: platform.to_string(),
            format: "gz".to_string(),
            size: None,
            sha256: None,
            created_at: None,
            updated_at: None,
            url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn list_versions_filters_channel_prerelease_search() {
        let index = MihomoIndex {
            schema_version: 1,
            source: None,
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            versions: vec![
                version("v1.0.0", "stable", false, vec![]),
                version("Prerelease-Alpha", "alpha", true, vec![]),
                version("v0.9.0", "stable", false, vec![]),
            ],
        };
        let tags = |filter: &VersionFilter| {
            list_versions(&index, Some(filter)).iter().map(|v| v.tag.as_str()).collect::<Vec<_>>()
        };

        assert_eq!(tags(&VersionFilter::default()), vec!["Prerelease-Alpha", "v1.0.0", "v0.9.0"]);
        assert_eq!(
            tags(&VersionFilter { channel: Some(Channel::Stable), ..Default::default() }),
            vec!["v1.0.0", "v0.9.0"]
        );
        assert_eq!(tags(&VersionFilter { prerelease: Some(true), ..Default::default() }), vec!["Prerelease-Alpha"]);
        assert_eq!(tags(&VersionFilter { search: Some("0.9".to_string()), ..Default::default() }), vec!["v0.9.0"]);
        assert_eq!(
            tags(&VersionFilter {
                channel: Some(Channel::Stable),
                search: Some("1.0".to_string()),
                ..Default::default()
            }),
            vec!["v1.0.0"]
        );
    }

    #[test]
    fn assets_for_platform_groups_all_assets_by_version() {
        let index = MihomoIndex {
            schema_version: 1,
            source: None,
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            versions: vec![
                version(
                    "v1.0.0",
                    "stable",
                    false,
                    vec![
                        asset("mihomo-linux-amd64-v1.0.0.gz", "linux-x86_64"),
                        asset("mihomo-linux-amd64-compatible-v1.0.0.gz", "linux-x86_64"),
                        asset("mihomo-darwin-arm64-v1.0.0.gz", "darwin-aarch64"),
                    ],
                ),
                version("v0.9.0", "stable", false, vec![asset("mihomo-linux-amd64-v0.9.0.gz", "linux-x86_64")]),
            ],
        };
        let versions = assets_for_platform(&index, Platform::LinuxX86_64, None);
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].tag, "v1.0.0");
        assert_eq!(
            versions[0].assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["mihomo-linux-amd64-v1.0.0.gz", "mihomo-linux-amd64-compatible-v1.0.0.gz"],
            "a version's assets must carry all of its assets for the platform"
        );
        assert_eq!(versions[1].tag, "v0.9.0");
        assert_eq!(versions[1].assets.len(), 1);
        // The darwin asset stays out of the linux group.
        assert_eq!(versions[0].assets.iter().filter(|a| a.platform == "darwin-aarch64").count(), 0);
        assert!(assets_for_platform(&index, Platform::WindowsX86_64, None).is_empty());
    }

    fn asset_by_name(name: &str) -> MihomoAsset {
        MihomoAsset {
            name: name.to_string(),
            platform: "linux-x86_64".to_string(),
            format: "gz".into(),
            size: Some(1),
            sha256: None,
            created_at: None,
            updated_at: None,
            url: format!("https://example.com/{name}"),
        }
    }

    fn sel_version() -> MihomoVersion {
        MihomoVersion {
            semver: Some("1.0.0".into()),
            tag: "v1.0.0".into(),
            prerelease: false,
            channel: "stable".to_string(),
            published_at: Some("2026-01-01T00:00:00Z".into()),
            created_at: None,
            updated_at: None,
            assets: vec![
                asset_by_name("mihomo-linux-amd64-v1.0.0.gz"),
                asset_by_name("mihomo-linux-amd64-compatible-v1.0.0.gz"),
            ],
        }
    }

    #[test]
    fn selects_asset_by_exact_name() {
        let v = sel_version();
        let a = select_asset_by_name(&v, "mihomo-linux-amd64-v1.0.0.gz").unwrap();
        assert_eq!(a.name, "mihomo-linux-amd64-v1.0.0.gz");
    }

    #[test]
    fn asset_not_found_without_name() {
        let v = sel_version();
        assert!(matches!(select_asset_by_name(&v, "mihomo-darwin-arm64-v1.0.0.gz"), Err(Error::AssetNotFound { .. })));
    }

    #[test]
    fn pick_asset_by_name_searches_newest_first() {
        let mut old = sel_version();
        old.tag = "v0.9.0".into();
        old.semver = Some("0.9.0".into());
        let newest = sel_version();
        let idx = MihomoIndex {
            schema_version: 1,
            source: None,
            generated_at: "2026-01-01T00:00:00Z".into(),
            versions: vec![old, newest],
        };
        // The asset exists in both versions; the newest must win.
        let a = pick_asset_by_name(&idx, "mihomo-linux-amd64-compatible-v1.0.0.gz", None).unwrap();
        assert_eq!(a.url, "https://example.com/mihomo-linux-amd64-compatible-v1.0.0.gz");
        // not present anywhere -> AssetNotFound
        assert!(matches!(
            pick_asset_by_name(&idx, "mihomo-darwin-amd64-v1.0.0.gz", None),
            Err(Error::AssetNotFound { .. })
        ));
    }

    #[test]
    fn pick_asset_by_name_pinned_version() {
        let mut old = sel_version();
        old.tag = "v0.9.0".into();
        old.semver = Some("0.9.0".into());
        let idx = MihomoIndex {
            schema_version: 1,
            source: None,
            generated_at: "2026-01-01T00:00:00Z".into(),
            versions: vec![old],
        };
        let a = pick_asset_by_name(&idx, "mihomo-linux-amd64-v1.0.0.gz", Some("0.9.0")).unwrap();
        assert_eq!(a.name, "mihomo-linux-amd64-v1.0.0.gz");
        // wrong version -> version not found error
        assert!(matches!(
            pick_asset_by_name(&idx, "mihomo-linux-amd64-v1.0.0.gz", Some("9.9.9")),
            Err(Error::AssetNotFound { .. })
        ));
    }
}
