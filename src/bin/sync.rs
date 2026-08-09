use std::path::{Path, PathBuf};

use clap::Parser;
use mihomo_versions::{
    CURRENT_SCHEMA_VERSION, Error, HttpClient, MihomoAsset, MihomoIndex, MihomoVersion, Source,
    classify::{ClassifierConfig, classify, mihomo_config, normalize_digest},
    normalize_tag, now_rfc3339, write_atomic,
};
use serde::Deserialize;

const DEFAULT_API_BASE: &str = "https://api.github.com";

#[derive(Parser)]
#[command(name = "mihomo-versions-sync", version, about = "Synchronize mihomo releases into a compact release index")]
struct Cli {
    /// Output index file path (single-repo mode).
    #[arg(long, default_value = "mihomo-releases.json")]
    out: PathBuf,
    /// Repository in `owner/name` form (single-repo mode).
    #[arg(long, default_value = "MetaCubeX/mihomo")]
    repo: String,
    /// GitHub token to raise the API rate limit.
    #[arg(long)]
    token: Option<String>,
    /// Keep only the N newest versions in the output.
    #[arg(long)]
    max_versions: Option<usize>,
    /// GitHub API base URL (primarily for tests).
    #[arg(long, hide = true)]
    api_base: Option<String>,
    /// Releases per page.
    #[arg(long, default_value = "100")]
    per_page: u32,
    /// Path to a classifier config JSON for a non-default repository.
    /// Defaults to the bundled MetaCubeX/mihomo rules.
    #[arg(long)]
    classifier: Option<PathBuf>,
    /// Batch mode: path to a sync config JSON describing multiple repositories.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Write the index as compact (single-line) JSON instead of pretty-printed.
    #[arg(long)]
    compact: bool,
    /// Write the index gzip-compressed instead of plain JSON (deprecated:
    /// prefer --emit-gz).
    #[arg(long)]
    gz: bool,
    /// Write both the plain JSON index and a gzip copy (appends `.gz` to the
    /// output path when missing). Applies to single-repo and batch modes.
    #[arg(long)]
    emit_gz: bool,
    /// Print the bundled default classifier config and exit.
    #[arg(long)]
    print_classifier: bool,
}

/// Batch sync configuration: one entry per repository. Tokens are not part of
/// the config (never commit secrets); use `--token` or the
/// `MIHOMO_VERSION_TOKEN` environment variable.
#[derive(Deserialize)]
struct BatchConfig {
    #[serde(default)]
    api_base: Option<String>,
    jobs: Vec<JobConfig>,
}

#[derive(Deserialize)]
struct JobConfig {
    repo: String,
    #[serde(default = "default_out")]
    out: String,
    #[serde(default)]
    classifier: Option<String>,
    #[serde(default)]
    max_versions: Option<usize>,
}

fn default_out() -> String {
    "mihomo-releases.json".to_string()
}

/// Appends `.gz` to the output path when gzip output is requested and the path
/// does not already end in `.gz`.
fn gz_path(out: &Path) -> PathBuf {
    if out.to_string_lossy().ends_with(".gz") {
        out.to_path_buf()
    } else {
        PathBuf::from(format!("{}.gz", out.display()))
    }
}

fn gzip_bytes(data: &[u8]) -> Result<Vec<u8>, Error> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    #[serde(default)]
    size: u64,
    browser_download_url: String,
    /// Per-asset digest from the GitHub API (e.g. `sha256:<hex>`), the source
    /// of the index's sha256.
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let cli = Cli::parse();
    if let Err(e) = run(&cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: &Cli) -> Result<(), Error> {
    if cli.print_classifier {
        println!("{}", serde_json::to_string_pretty(&mihomo_config())?);
        return Ok(());
    }
    if let Some(config_path) = &cli.config {
        if cli.repo != "MetaCubeX/mihomo" || cli.out.as_os_str() != "mihomo-releases.json" {
            log::warn!("--repo/--out are ignored in batch (--config) mode");
        }
        return run_batch(cli, config_path).await;
    }
    let client = client_with_token(cli.token.as_deref())?;
    let out = resolve_project_path(&cli.out);
    let classifier_path = cli.classifier.as_deref().map(resolve_project_path);
    let classifier = load_classifier(classifier_path.as_deref())?;
    let opts = SyncOptions {
        api_base: cli.api_base.clone().unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
        per_page: cli.per_page,
        max_versions: cli.max_versions,
        compact: cli.compact,
        emit_gz: cli.emit_gz,
        gz_only: cli.gz && !cli.emit_gz,
    };
    if cli.gz {
        log::warn!("--gz is deprecated; use --emit-gz to write both JSON and gzip");
    }
    log::info!("syncing {} -> {}", cli.repo, out.display());
    sync_repo(&client, &classifier, &cli.repo, &out, &opts).await?;
    Ok(())
}

/// Builds an HTTP client using the CLI `--token` or the `MIHOMO_VERSION_TOKEN`
/// environment variable (CLI wins). No token -> unauthenticated client.
fn client_with_token(cli_token: Option<&str>) -> Result<HttpClient, Error> {
    let token =
        cli_token.map(str::to_string).or_else(|| std::env::var("MIHOMO_VERSION_TOKEN").ok().filter(|t| !t.is_empty()));
    match token {
        Some(token) => HttpClient::with_token(&token),
        None => HttpClient::new(),
    }
}

/// Batch mode: syncs every job in the config, running all of them even when
/// individual jobs fail (invalid repo, classifier load failure, or sync
/// failure), and exiting non-zero if any failed.
async fn run_batch(cli: &Cli, config_path: &Path) -> Result<(), Error> {
    let config_path = resolve_project_path(config_path);
    let text = tokio::fs::read_to_string(&config_path).await?;
    let config: BatchConfig = serde_json::from_str(&text)?;
    if config.jobs.is_empty() {
        return Err(Error::InvalidSchema("sync config requires at least one job".to_string()));
    }
    let api_base = cli.api_base.as_deref().or(config.api_base.as_deref()).unwrap_or(DEFAULT_API_BASE);
    let client = client_with_token(cli.token.as_deref())?;
    if cli.gz {
        log::warn!("--gz is deprecated; use --emit-gz to write both JSON and gzip");
    }

    let mut failed: Vec<(String, Error)> = Vec::new();
    for job in &config.jobs {
        if parse_repo(&job.repo).is_none() {
            let e = Error::InvalidSchema(format!("expected owner/name, got {:?}", job.repo));
            log::warn!("skipping invalid repo {}: {e}", job.repo);
            failed.push((job.repo.clone(), e));
            continue;
        }
        log::info!("syncing {}", job.repo);
        let out = resolve_project_path(Path::new(&job.out));
        let classifier_path = job.classifier.as_deref().map(|c| resolve_project_path(Path::new(c)));
        let classifier = match load_classifier(classifier_path.as_deref()) {
            Ok(classifier) => classifier,
            Err(e) => {
                log::warn!("skipping {}: classifier load failed: {e}", job.repo);
                failed.push((job.repo.clone(), e));
                continue;
            }
        };
        let opts = SyncOptions {
            api_base: api_base.to_string(),
            per_page: cli.per_page,
            max_versions: job.max_versions,
            compact: cli.compact,
            emit_gz: cli.emit_gz,
            gz_only: cli.gz && !cli.emit_gz,
        };
        match sync_repo(&client, &classifier, &job.repo, &out, &opts).await {
            Ok(stats) => log::info!(
                "synced {} ({} releases, {} versions, {} assets dropped) -> {}",
                job.repo,
                stats.releases,
                stats.versions,
                stats.dropped_assets,
                out.display()
            ),
            Err(e) => {
                log::warn!("sync failed for {}: {e}", job.repo);
                failed.push((job.repo.clone(), e));
            }
        }
    }

    if failed.is_empty() {
        return Ok(());
    }
    for (repo, e) in &failed {
        eprintln!("{repo}: {e}");
    }
    Err(Error::InvalidSchema(format!("{} job(s) failed", failed.len())))
}

struct SyncStats {
    releases: usize,
    versions: usize,
    dropped_assets: usize,
}

/// Options for a single `sync_repo` run.
struct SyncOptions {
    api_base: String,
    per_page: u32,
    max_versions: Option<usize>,
    /// Write single-line JSON instead of pretty-printed.
    compact: bool,
    /// Write both plain JSON and a `.gz` copy.
    emit_gz: bool,
    /// Write gzip only (deprecated `--gz` mode).
    gz_only: bool,
}

/// Syncs a single repository. Incremental: releases whose `updated_at` (and
/// whose kept assets' `updated_at`, `platform`, `format`, and `size`) are
/// unchanged are reused from the existing index; changed, new, or deleted
/// releases are reprocessed / dropped.
async fn sync_repo(
    client: &HttpClient,
    classifier: &ClassifierConfig,
    repo: &str,
    out: &Path,
    opts: &SyncOptions,
) -> Result<SyncStats, Error> {
    let started = std::time::Instant::now();
    let previous = load_previous_index(out).await;
    let mut versions: Vec<MihomoVersion> = Vec::new();
    let mut page: u32 = 1;
    let mut fetched_releases = 0usize;
    let mut reused_versions = 0usize;
    let mut dropped_assets = 0usize;

    loop {
        let url = format!(
            "{}/repos/{repo}/releases?per_page={}&page={}",
            opts.api_base.trim_end_matches('/'),
            opts.per_page,
            page
        );
        log::info!("fetching page {page} ...");
        let releases: Vec<GhRelease> = client.get_json(&url).await?;
        if releases.is_empty() {
            break;
        }
        for release in releases {
            if release.draft {
                log::debug!("skipping draft release {}", release.tag_name);
                continue;
            }
            fetched_releases += 1;
            // Classify each asset exactly once per release; the result feeds
            // both the incremental reuse check and the assembled entry.
            let (assets, dropped) = kept_assets(classifier, &release);
            if let Some(prev) =
                previous.as_ref().and_then(|index| index.versions.iter().find(|v| v.tag == release.tag_name))
            {
                if prev.updated_at == release.updated_at
                    && prev.semver == normalize_tag(&release.tag_name)
                    && prev.channel == version_channel(&release.tag_name, release.prerelease)
                    && prev.prerelease == release.prerelease
                    && prev.published_at == release.published_at
                    && assets_unchanged(prev, &assets)
                {
                    log::debug!(
                        "release {} unchanged (updated_at + derived fields + kept assets); reusing from previous index",
                        release.tag_name
                    );
                    reused_versions += 1;
                    versions.push(prev.clone());
                    continue;
                }
                log::debug!(
                    "release {} changed (updated_at, derived fields, or kept assets); reprocessing",
                    release.tag_name
                );
            }
            log::debug!(
                "processing release {} (prerelease={}, {} assets)",
                release.tag_name,
                release.prerelease,
                release.assets.len()
            );
            dropped_assets += dropped;
            log::debug!("release {} -> {} assets kept, {dropped} dropped", release.tag_name, assets.len());
            versions.push(build_version(&release, assets));
        }
        log::info!(
            "page {page}: {fetched_releases} releases fetched ({} reused, {} assets dropped so far)",
            reused_versions,
            dropped_assets
        );
        page += 1;
    }

    if let Some(limit) = opts.max_versions {
        versions.truncate(limit);
    }

    // Keep the previous `generated_at` when the release data did not change,
    // so the output is byte-identical and the workflow's git-diff check skips
    // the commit/upload when nothing moved. It then reads as "last time this
    // content changed".
    let generated_at = match previous.as_ref().filter(|prev| prev.versions == versions) {
        Some(prev) => prev.generated_at.clone(),
        None => now_rfc3339(),
    };

    let index = MihomoIndex {
        schema_version: CURRENT_SCHEMA_VERSION,
        source: parse_repo(repo).map(|(owner, repo)| Source { owner, repo }),
        generated_at,
        versions,
    };

    log::info!("writing index: {} versions (emit_gz={}, gz_only={})", index.versions.len(), opts.emit_gz, opts.gz_only);
    let json = if opts.compact { serde_json::to_string(&index)? } else { serde_json::to_string_pretty(&index)? };
    let json_bytes = json.into_bytes();
    if opts.emit_gz || opts.gz_only {
        let gz_out = gz_path(out);
        write_atomic(&gz_out, &gzip_bytes(&json_bytes)?)?;
        log::info!("wrote gzip index -> {}", gz_out.display());
    }
    if !opts.gz_only {
        write_atomic(out, &json_bytes)?;
        log::info!("wrote index -> {}", out.display());
    }
    log::info!(
        "synced {fetched_releases} releases ({} versions, {reused_versions} reused, {dropped_assets} assets dropped) in {:.1}s",
        index.versions.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(SyncStats { releases: fetched_releases, versions: index.versions.len(), dropped_assets })
}

/// Loads the previously written plain-JSON index for incremental reuse, if it
/// exists and parses. Returns `None` otherwise.
async fn load_previous_index(out: &Path) -> Option<MihomoIndex> {
    if !out.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(out).ok()?;
    match serde_json::from_str::<MihomoIndex>(&text) {
        Ok(index) => Some(index),
        Err(e) => {
            log::warn!("could not parse existing index {}; treating all releases as new: {e}", out.display());
            None
        }
    }
}

/// True when the classifier-kept assets of `release` exactly match the kept
/// assets of the previous index entry — the same names in both directions with
/// the same per-asset `updated_at`, and identical classifier-derived fields
/// (`platform`, `format`) and `size` — so the whole entry can be reused.
/// Comparing the derived fields means a classifier rule change (canonical
/// platform name, format mapping) reprocesses instead of silently reusing
/// stale values. Assets the classifier drops (android builds, source
/// archives, checksums, ...) are ignored; a kept asset that was added or
/// removed on GitHub counts as changed.
///
/// This only covers the asset-level comparison. The caller additionally
/// compares the version-level fields (`semver`, `channel`, `prerelease`,
/// `published_at`) against freshly derived values, so a version-classification
/// rule change (channel mapping, tag normalization) also reprocesses instead
/// of reusing stale entries.
fn assets_unchanged(prev: &MihomoVersion, kept: &[MihomoAsset]) -> bool {
    kept.len() == prev.assets.len()
        && kept.iter().all(|asset| {
            prev.assets.iter().find(|a| a.name == asset.name).is_some_and(|a| {
                a.updated_at == asset.updated_at
                    && a.platform == asset.platform
                    && a.format == asset.format
                    && a.size == asset.size
            })
        })
}

/// Classifies every asset of `release` exactly once, returning the kept
/// assets (enriched with digest and API timestamps) and the number dropped.
/// Both the incremental reuse check and the assembled index entry consume
/// this single pass, so `classify` runs once per release.
fn kept_assets(classifier: &ClassifierConfig, release: &GhRelease) -> (Vec<MihomoAsset>, usize) {
    let mut assets = Vec::with_capacity(release.assets.len());
    let mut dropped = 0usize;
    for asset in &release.assets {
        match classify(classifier, &asset.name, &asset.browser_download_url, asset.size) {
            Some(mut indexed) => {
                indexed.sha256 = match asset.digest.as_deref() {
                    Some(raw) => match normalize_digest(raw) {
                        Some(hex) => Some(hex),
                        None => {
                            log::warn!(
                                "asset {} has malformed digest {:?}; download checksum verification will be skipped",
                                asset.name,
                                raw
                            );
                            None
                        }
                    },
                    None => None,
                };
                indexed.created_at = asset.created_at.clone();
                indexed.updated_at = asset.updated_at.clone();
                assets.push(indexed);
            }
            None => dropped += 1,
        }
    }
    (assets, dropped)
}

fn build_version(release: &GhRelease, assets: Vec<MihomoAsset>) -> MihomoVersion {
    MihomoVersion {
        semver: normalize_tag(&release.tag_name),
        tag: release.tag_name.clone(),
        prerelease: release.prerelease,
        channel: version_channel(&release.tag_name, release.prerelease).to_string(),
        published_at: release.published_at.clone(),
        created_at: release.created_at.clone(),
        updated_at: release.updated_at.clone(),
        assets,
    }
}

/// Classifies a release into a distribution channel: tags containing
/// `nightly` are `nightly`; prerelease builds or `alpha` tags are `alpha`;
/// everything else is `stable`.
fn version_channel(tag: &str, prerelease: bool) -> &'static str {
    let lower = tag.to_ascii_lowercase();
    if lower.contains("nightly") {
        "nightly"
    } else if prerelease || lower.contains("alpha") {
        "alpha"
    } else {
        "stable"
    }
}

fn load_classifier(path: Option<&Path>) -> Result<ClassifierConfig, Error> {
    let config = match path {
        Some(path) => {
            let text = std::fs::read_to_string(path)?;
            serde_json::from_str(&text)?
        }
        None => mihomo_config(),
    };
    config.validate()?;
    Ok(config)
}

fn parse_repo(repo: &str) -> Option<(String, String)> {
    let (owner, name) = repo.split_once('/')?;
    Some((owner.to_string(), name.to_string()))
}

/// Resolves a path given as a CLI argument: absolute paths and relative paths
/// that already exist in the current directory are used as-is; other relative
/// paths resolve against the crate's project root (`CARGO_MANIFEST_DIR`).
fn resolve_project_path(path: &Path) -> PathBuf {
    if path.is_absolute() || path.is_file() {
        path.to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_absolute_paths() {
        let abs = Path::new("/tmp/out/index.json");
        assert_eq!(resolve_project_path(abs), abs);
    }

    #[test]
    fn resolves_missing_relative_paths_against_project_root() {
        let resolved = resolve_project_path(Path::new("some/dir/index.json"));
        assert_eq!(resolved, Path::new(env!("CARGO_MANIFEST_DIR")).join("some/dir/index.json"));
    }

    #[test]
    fn keeps_existing_relative_paths() {
        let probe = Path::new("target/resolve_project_path_probe.tmp");
        std::fs::create_dir_all("target").unwrap();
        std::fs::write(probe, b"x").unwrap();
        assert_eq!(resolve_project_path(probe), probe);
        let _ = std::fs::remove_file(probe);
    }
}
