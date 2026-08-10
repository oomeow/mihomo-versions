use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::{client::HttpClient, error::Error, model::MihomoAsset};

/// Attempts per download before giving up (1 initial + 2 retries).
const MAX_ATTEMPTS: u32 = 3;

/// Options controlling a single download.
#[derive(Clone)]
pub struct DownloadOptions {
    /// When set and cancelled, the download stops with `Error::Cancelled` and
    /// the partial `.part` file is kept so a later run can resume it.
    pub cancel: Option<CancellationToken>,
    /// Maximum time without receiving any data before giving up with
    /// `Error::Timeout` (retryable).
    pub idle_timeout: Option<Duration>,
    /// Maximum total time for a single download attempt, from request start to
    /// the last body byte. When exceeded, the attempt fails with
    /// `Error::Timeout` (retryable). `None` (the default) leaves the attempt
    /// unbounded in total duration — `idle_timeout` and resume still guard
    /// against stalled connections.
    pub total_timeout: Option<Duration>,
    /// Resume an existing `.part` file via HTTP `Range` requests. Correctness is
    /// guaranteed because the resumed bytes are still sha256-verified.
    pub resume: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self { cancel: None, idle_timeout: None, total_timeout: None, resume: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    Gz,
    Zip,
    /// `.tar.gz`: gzip-decompress then extract the binary from the tar archive.
    TarGz,
    /// `.zst`: zstd-decompress the single binary.
    Zst,
    /// Not an archive: the downloaded bytes are copied to `dest` as-is.
    Raw,
}

impl ArchiveFormat {
    fn parse(s: &str) -> Self {
        match s {
            "gz" => ArchiveFormat::Gz,
            "zip" => ArchiveFormat::Zip,
            "tar.gz" => ArchiveFormat::TarGz,
            "zst" => ArchiveFormat::Zst,
            _ => ArchiveFormat::Raw,
        }
    }
}

/// Downloads `asset` into `dest`, streaming to a temporary `.part` file,
/// verifying the archive's SHA-256 (when the index carries one), and
/// decompressing to the final path.
///
/// `progress` is called with `(downloaded_bytes, total_bytes)`: the first
/// argument is the number of bytes downloaded so far, the second the expected
/// total (`0` when unknown).
///
/// The SHA-256 in the index is the digest of the uploaded archive (taken from
/// the GitHub API's asset `digest`), so verification runs on the archive bytes
/// before decompression. When the digest is missing, verification is skipped
/// with a warning. `options.total_timeout` bounds a single attempt; when
/// `None`, only `idle_timeout` (and resume) guard the transfer.
pub async fn download(
    client: &HttpClient,
    asset: &MihomoAsset,
    dest: &Path,
    options: DownloadOptions,
    progress: impl Fn(u64, u64),
) -> Result<(), Error> {
    let format = ArchiveFormat::parse(&asset.format);
    log::debug!("downloading {} ({}) -> {}", asset.name, asset.format, dest.display());
    for attempt in 1..=MAX_ATTEMPTS {
        match attempt_download(client, asset, dest, format, &options, &progress).await {
            Ok(()) => {
                log::debug!("download complete: {} -> {}", asset.name, dest.display());
                return Ok(());
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) if attempt < MAX_ATTEMPTS && is_retryable(&e) => {
                log::warn!("download attempt {attempt}/{MAX_ATTEMPTS} failed: {e}; retrying");
                tokio::time::sleep(Duration::from_millis(500 * (1 << (attempt - 1)))).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Spawns a download on the current tokio runtime and returns immediately with a
/// [`DownloadHandle`] that can cancel it and await its result.
///
/// `asset` and `dest` are moved into the background task; `client` is cloned.
/// If `options.cancel` is `None`, an internal cancellation token is created and
/// exposed through the handle.
///
/// `progress` is called with `(downloaded_bytes, total_bytes)`: the first
/// argument is the number of bytes downloaded so far, the second the expected
/// total (`0` when unknown).
pub fn download_async(
    client: &HttpClient,
    asset: MihomoAsset,
    dest: PathBuf,
    options: DownloadOptions,
    progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> DownloadHandle {
    let token = options.cancel.clone().unwrap_or_default();
    let options = DownloadOptions { cancel: Some(token.clone()), ..options };
    let client = client.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = download(&client, &asset, &dest, options, progress).await;
        let _ = tx.send(result);
    });
    DownloadHandle { token, result: rx }
}

/// A running background download started by [`download_async`].
pub struct DownloadHandle {
    token: CancellationToken,
    result: tokio::sync::oneshot::Receiver<Result<(), Error>>,
}

impl DownloadHandle {
    /// Cancels the download; `wait` then returns `Error::Cancelled`.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Waits for the background download to finish and returns its result.
    pub async fn wait(self) -> Result<(), Error> {
        match self.result.await {
            Ok(result) => result,
            Err(_) => Err(Error::Io(std::io::Error::other("download task ended without a result"))),
        }
    }
}

/// 单次下载尝试（含重试逻辑由 [`download`] 负责）。
///
/// `progress` 回调参数为 `(downloaded_bytes, total_bytes)`：已下载字节数 / 总字节数（`0` 表示未知）。
async fn attempt_download(
    client: &HttpClient,
    asset: &MihomoAsset,
    dest: &Path,
    format: ArchiveFormat,
    options: &DownloadOptions,
    progress: &impl Fn(u64, u64),
) -> Result<(), Error> {
    match options.total_timeout {
        Some(timeout) => {
            let result =
                tokio::time::timeout(timeout, attempt_download_inner(client, asset, dest, format, options, progress))
                    .await;
            match result {
                Ok(result) => result,
                Err(_) => {
                    log::warn!("download {} exceeded total_timeout {timeout:?}", asset.name);
                    Err(Error::Timeout)
                }
            }
        }
        None => attempt_download_inner(client, asset, dest, format, options, progress).await,
    }
}

async fn attempt_download_inner(
    client: &HttpClient,
    asset: &MihomoAsset,
    dest: &Path,
    format: ArchiveFormat,
    options: &DownloadOptions,
    progress: &impl Fn(u64, u64),
) -> Result<(), Error> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = temp_part_path(dest);

    let existing = if options.resume { existing_part_bytes(&part, asset).await } else { 0 };
    log::debug!("downloading {}: existing .part bytes = {existing}", asset.name);

    // Open the right request and set up the write offset.
    let (mut response, offset, total): (reqwest::Response, u64, u64) = if existing > 0 {
        let resp = client.open_range(&asset.url, existing).await.map_err(|e| missing_asset(asset, e))?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {
                let remaining = resp.content_length().unwrap_or(0);
                let total = existing + remaining;
                log::debug!("resuming {} from {existing} (206), total {total}", asset.name);
                (resp, existing, total)
            }
            StatusCode::OK => {
                log::debug!("server ignored Range for {}; restarting from 0", asset.name);
                let total = resp.content_length().unwrap_or(0);
                (resp, 0, total)
            }
            StatusCode::RANGE_NOT_SATISFIABLE => {
                log::debug!("{} .part already complete (416); skipping download", asset.name);
                let mut hasher = Sha256::new();
                hash_file_into(&part, &mut hasher).await?;
                return verify_and_extract(&part, dest, format, hasher, asset).await;
            }
            other => return Err(missing_asset(asset, crate::client::status_to_error(other))),
        }
    } else {
        let resp = client.open(&asset.url).await.map_err(|e| missing_asset(asset, e))?;
        let total = resp.content_length().unwrap_or(0);
        (resp, 0, total)
    };

    let mut file = if offset > 0 {
        tokio::fs::OpenOptions::new().append(true).open(&part).await?
    } else {
        tokio::fs::File::create(&part).await?
    };
    write_part_meta(&part, asset).await?;

    // Streaming sha256; seed with the already-downloaded bytes when resuming.
    let mut hasher = Sha256::new();
    if offset > 0 {
        hash_file_into(&part, &mut hasher).await?;
    }

    let mut written: u64 = 0;
    let result: Result<(), Error> = loop {
        // Deterministic check: a token cancelled before or between chunks wins
        // over an eagerly-ready body (tokio::select! is unbiased and could pick
        // the read branch). In-flight cancellations are still aborted inside
        // read_chunk via the select race.
        if options.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            break Err(Error::Cancelled);
        }
        let chunk = match read_chunk(&mut response, options.cancel.as_ref(), options.idle_timeout).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        file.write_all(&chunk).await?;
        hasher.update(&chunk);
        written += chunk.len() as u64;
        progress(offset + written, total);
    };
    let _ = file.flush().await;
    drop(file);

    if let Err(e) = result {
        log::debug!("download aborted after {written} new bytes: {e}");
        return Err(e);
    }
    let total = offset + written;
    log::debug!("downloaded {written} bytes (total {total}) to {}", part.display());

    verify_and_extract(&part, dest, format, hasher, asset).await
}

fn temp_part_path(dest: &Path) -> PathBuf {
    let name = dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "download".into());
    dest.with_file_name(format!("{name}.part"))
}

/// Sidecar recording which asset a `.part` file belongs to, so a partial from a
/// previously downloaded (different) asset is never resumed by mistake.
fn meta_path(part: &Path) -> PathBuf {
    part.with_extension("part.meta")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PartMeta {
    url: String,
}

/// Returns the byte size of an existing `.part`, treating a partial left by a
/// *different* asset (per the sidecar URL) as stale: it is discarded so the new
/// asset downloads from scratch instead of resuming corrupt bytes.
async fn existing_part_bytes(part: &Path, asset: &MihomoAsset) -> u64 {
    let size = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return 0;
    }
    let meta =
        tokio::fs::read(meta_path(part)).await.ok().and_then(|bytes| serde_json::from_slice::<PartMeta>(&bytes).ok());
    if meta.is_none_or(|m| m.url != asset.url) {
        log::warn!("discarding stale partial for {} (belongs to another asset)", part.display());
        let _ = tokio::fs::remove_file(part).await;
        return 0;
    }
    size
}

async fn write_part_meta(part: &Path, asset: &MihomoAsset) -> Result<(), Error> {
    let meta = serde_json::to_string(&PartMeta { url: asset.url.clone() })?;
    tokio::fs::write(meta_path(part), meta).await?;
    Ok(())
}

/// Maps a `tokio::time::timeout` result around a chunk read into the library
/// error type (idle timeout -> `Error::Timeout`).
fn map_timed(
    res: Result<Result<Option<bytes::Bytes>, reqwest::Error>, tokio::time::error::Elapsed>,
) -> Result<Option<bytes::Bytes>, Error> {
    match res {
        Ok(res) => res.map_err(Error::Network),
        Err(_) => Err(Error::Timeout),
    }
}

/// Reads the next response chunk, racing the cancellation token against the
/// read (and the idle timeout) so a cancellation aborts an in-flight `chunk()`
/// immediately instead of waiting for the next loop iteration.
async fn read_chunk(
    response: &mut reqwest::Response,
    cancel: Option<&CancellationToken>,
    idle_timeout: Option<Duration>,
) -> Result<Option<bytes::Bytes>, Error> {
    match cancel {
        Some(cancel) => {
            let cancelled = cancel.cancelled();
            tokio::pin!(cancelled);
            let read = response.chunk();
            tokio::pin!(read);
            match idle_timeout {
                Some(timeout) => {
                    let timed = tokio::time::timeout(timeout, read);
                    tokio::pin!(timed);
                    tokio::select! {
                        _ = &mut cancelled => Err(Error::Cancelled),
                        res = &mut timed => map_timed(res),
                    }
                }
                None => tokio::select! {
                    _ = &mut cancelled => Err(Error::Cancelled),
                    res = &mut read => res.map_err(Error::Network),
                },
            }
        }
        None => match idle_timeout {
            Some(timeout) => map_timed(tokio::time::timeout(timeout, response.chunk()).await),
            None => response.chunk().await.map_err(Error::Network),
        },
    }
}

async fn hash_file_into(path: &Path, hasher: &mut Sha256) -> Result<(), Error> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(())
}

/// Removes the partial file and its sidecar meta together.
async fn cleanup_part(part: &Path) {
    let _ = tokio::fs::remove_file(part).await;
    let _ = tokio::fs::remove_file(meta_path(part)).await;
}

async fn verify_and_extract(
    part: &Path,
    dest: &Path,
    format: ArchiveFormat,
    hasher: Sha256,
    asset: &MihomoAsset,
) -> Result<(), Error> {
    let actual = hex::encode(hasher.finalize());
    match asset.sha256.as_deref() {
        Some(expected) => {
            if actual.eq_ignore_ascii_case(expected) {
                log::debug!("sha256 verified: {actual}");
            } else {
                log::warn!("checksum mismatch for {}; removing partial file", part.display());
                cleanup_part(part).await;
                return Err(Error::ChecksumMismatch { expected: expected.to_string(), actual });
            }
        }
        None => log::warn!("asset {} has no SHA-256 in the index; skipping verification", asset.name),
    }

    log::debug!("extracting {} ({format:?})", part.display());
    let result = extract(part, dest, format).await;
    cleanup_part(part).await;
    result
}

async fn extract(part: &Path, dest: &Path, format: ArchiveFormat) -> Result<(), Error> {
    let part = part.to_path_buf();
    let dest = dest.to_path_buf();
    let handle = tokio::task::spawn_blocking(move || match format {
        ArchiveFormat::Gz => extract_gz(&part, &dest),
        ArchiveFormat::Zip => extract_zip(&part, &dest),
        ArchiveFormat::TarGz => extract_tar_gz(&part, &dest),
        ArchiveFormat::Zst => extract_zst(&part, &dest),
        ArchiveFormat::Raw => std::fs::rename(&part, &dest).map_err(Error::from),
    });
    handle.await.map_err(io_err)?
}

fn extract_zst(part: &Path, dest: &Path) -> Result<(), Error> {
    let source = std::fs::File::open(part)?;
    // `Decoder::new` is lazy: frame errors only surface on read, so every read
    // error is a decompression failure and maps to `ArchiveContent`.
    let mut decoder = zstd::stream::read::Decoder::new(source).map_err(|e| Error::ArchiveContent(e.to_string()))?;
    let result = (|| -> Result<(), Error> {
        let mut out = std::fs::File::create(dest)?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = decoder.read(&mut buf).map_err(|e| Error::ArchiveContent(e.to_string()))?;
            if n == 0 {
                return Ok(());
            }
            std::io::Write::write_all(&mut out, &buf[..n])?;
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn extract_gz(part: &Path, dest: &Path) -> Result<(), Error> {
    let source = std::fs::File::open(part)?;
    let mut decoder = flate2::read::GzDecoder::new(source);
    let result = write_output(&mut decoder, dest);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn extract_zip(part: &Path, dest: &Path) -> Result<(), Error> {
    let file = std::fs::File::open(part)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let entry_name = pick_zip_entry(&archive)?;
    let mut entry = archive.by_name(&entry_name).map_err(|_| Error::ArchiveContent(entry_name.clone()))?;
    let result = write_output(&mut entry, dest);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// Extracts the binary from a `.tar.gz` archive. The `dest` basename is the
/// preferred entry name (e.g. `--dest ./target/meow` picks a `meow` entry);
/// otherwise the first regular file is used. Single-pass: the archive is
/// decompressed once (a second pass only happens when a preferred entry exists
/// but is not found, falling back to the first regular file).
fn extract_tar_gz(part: &Path, dest: &Path) -> Result<(), Error> {
    let preferred = dest.file_name().map(|s| s.to_string_lossy().into_owned());
    let file = std::fs::File::open(part)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut first_file: Option<String> = None;
    for entry in archive.entries().map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry.path().map_err(io_err)?.to_string_lossy().into_owned();
        if first_file.is_none() {
            first_file = Some(name.clone());
        }
        if preferred.as_deref().is_some_and(|p| Path::new(&name).file_name().is_some_and(|b| b == p)) {
            return extract_tar_entry(entry, dest);
        }
        if preferred.is_none() {
            return extract_tar_entry(entry, dest);
        }
    }
    // The preferred entry was not found; fall back to the first regular file
    // (a second decompression pass, but only in this fallback case).
    let entry_name =
        first_file.ok_or_else(|| Error::ArchiveContent("tar.gz archive contains no regular file".to_string()))?;
    let file = std::fs::File::open(part)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        if entry.path().map_err(io_err)?.to_string_lossy() == entry_name {
            return extract_tar_entry(entry, dest);
        }
    }
    Err(Error::ArchiveContent(entry_name))
}

fn extract_tar_entry<R: Read>(mut entry: tar::Entry<R>, dest: &Path) -> Result<(), Error> {
    let result = write_output(&mut entry, dest);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn write_output<R: Read>(reader: &mut R, dest: &Path) -> Result<(), Error> {
    let mut output = std::fs::File::create(dest)?;
    std::io::copy(reader, &mut output)?;
    Ok(())
}

/// Maps a tar/IO error into the library error type.
fn io_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> Error {
    Error::Io(std::io::Error::other(e))
}

fn pick_zip_entry(archive: &zip::ZipArchive<std::fs::File>) -> Result<String, Error> {
    let names: Vec<String> = (0..archive.len()).filter_map(|i| archive.name_for_index(i).map(str::to_string)).collect();
    for wanted in ["mihomo.exe", "mihomo"] {
        if let Some(name) = names.iter().find(|n| n.ends_with(wanted)) {
            return Ok(name.clone());
        }
    }
    names
        .iter()
        .find(|n| !n.ends_with('/'))
        .cloned()
        .ok_or_else(|| Error::ArchiveContent("zip archive is empty".to_string()))
}

fn is_retryable(e: &Error) -> bool {
    match e {
        Error::Network(_) | Error::Timeout => true,
        Error::Http(status) => *status >= 500,
        _ => false,
    }
}

/// Maps a bare HTTP 404 to an error that names the asset and its URL, so the
/// user sees *what* is gone instead of a bare status code.
fn missing_asset(asset: &MihomoAsset, e: Error) -> Error {
    match e {
        Error::Http(404) => Error::AssetUnavailable { name: asset.name.clone(), url: asset.url.clone() },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_enable_resume() {
        assert!(DownloadOptions::default().resume);
        assert!(DownloadOptions::default().cancel.is_none());
        assert!(DownloadOptions::default().idle_timeout.is_none());
    }

    #[test]
    fn part_path_is_deterministic() {
        let dest = Path::new("/tmp/out/mihomo");
        assert_eq!(temp_part_path(dest), Path::new("/tmp/out/mihomo.part"));
    }
}
