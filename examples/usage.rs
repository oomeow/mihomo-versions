//! End-to-end usage example for `mihomo-versions`.
//!
//! Demonstrates the full client flow: load the version index, detect the
//! current platform, pick an asset **by its exact name**, and download +
//! verify it.
//!
//! Usage:
//! ```text
//! cargo run --example usage -- <source> --asset-name <name> [--version <semver|tag>] [--dest <path>] [--dry-run]
//! ```
//!
//! `source` is either:
//! - the index JSON file URL (fetched over HTTP, e.g. `https://your-cdn/.../mihomo-releases.json`), or
//! - a path to a local `mihomo-releases.json` for offline inspection.
//!
//! `--asset-name` is required: assets are selected by exact name (searched
//! newest-first unless `--version` pins one). Use `--dry-run` to only print
//! the selection without downloading.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use mihomo_versions::{
    Channel, DownloadOptions, Error, HttpClient, MihomoIndex, Platform, download, fetch_index, pick_asset_by_name,
    sorted_versions,
};

const USAGE: &str = "\
usage: usage <source> --asset-name <name> [--version <semver|tag>] [--channel <stable|alpha|nightly>] [--dest <path>] [--dry-run]

source: index JSON file URL (HTTP) or a local mihomo-releases.json file.
";

struct Args {
    source: String,
    version: Option<String>,
    asset_name: Option<String>,
    dest: Option<PathBuf>,
    index_cache: Option<PathBuf>,
    channel: Option<Channel>,
    dry_run: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut source: Option<String> = None;
    let mut version: Option<String> = None;
    let mut asset_name: Option<String> = None;
    let mut dest: Option<PathBuf> = None;
    let mut index_cache: Option<PathBuf> = None;
    let mut channel: Option<Channel> = None;
    let mut dry_run = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" => version = it.next(),
            "--asset-name" => asset_name = it.next(),
            "--dest" => dest = it.next().map(PathBuf::from),
            "--index-cache" => index_cache = it.next().map(PathBuf::from),
            "--channel" => {
                let name = it.next().ok_or_else(|| format!("missing value for --channel\n{USAGE}"))?;
                channel = Some(
                    Channel::parse(&name)
                        .ok_or_else(|| format!("unknown channel {name} (stable|alpha|nightly)\n{USAGE}"))?,
                );
            }
            "--dry-run" => dry_run = true,
            "--help" | "-h" => return Err(USAGE.to_string()),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}\n{USAGE}")),
            other => source = Some(other.to_string()),
        }
    }
    Ok(Args {
        source: source.ok_or_else(|| format!("missing source\n{USAGE}"))?,
        version,
        asset_name,
        dest,
        index_cache,
        channel,
        dry_run,
    })
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprint!("{message}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&args).await {
        eprintln!("example failed: {e}");
        std::process::exit(1);
    }
}

async fn run(args: &Args) -> Result<(), Error> {
    let index = load_index(args).await?;
    // Narrow the index to the requested channel first, so both the version
    // listing and the asset selection operate on the same set.
    let index = match args.channel {
        Some(channel) => {
            let mut narrowed = index.clone();
            narrowed.versions.retain(|v| v.channel == channel.as_str());
            println!("filtering to channel: {}", channel.as_str());
            narrowed
        }
        None => index,
    };
    println!(
        "index: schema_version={} generated_at={} versions={}",
        index.schema_version,
        index.generated_at,
        index.versions.len()
    );

    let platform = Platform::current()?;
    println!("current platform: {platform}");

    println!("\ntop 5 versions:");
    let versions = sorted_versions(&index);
    if versions.is_empty() {
        println!("  (no versions)");
    }
    for version in versions.into_iter().take(5) {
        let semver = version.semver.as_deref().unwrap_or("<latest build>");
        println!("  {} ({semver}, channel: {})", version.tag, version.channel);
    }

    let asset = match &args.asset_name {
        Some(name) => pick_asset_by_name(&index, name, args.version.as_deref()),
        None => {
            eprintln!("missing --asset-name; an asset is selected by its exact name\n{USAGE}");
            std::process::exit(2);
        }
    };
    let asset = match asset {
        Ok(asset) => asset,
        Err(Error::AssetNotFound { name, version }) => {
            // Report the actual search scope (channel narrowing / version pin).
            let scope = match (&args.channel, version.as_deref()) {
                (Some(channel), Some(v)) => format!("version {v} of channel {}", channel.as_str()),
                (Some(channel), None) => format!("channel {}", channel.as_str()),
                (None, Some(v)) => format!("version {v}"),
                (None, None) => "any version".to_string(),
            };
            println!("\nno asset named {name} in {scope}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    println!("\nselected asset: {}", asset.name);
    println!(
        "  platform={} format={} size={}",
        asset.platform,
        asset.format,
        asset.size.map_or("unknown".to_string(), |s| s.to_string())
    );
    match &asset.sha256 {
        Some(digest) => println!("  sha256: {digest}"),
        None => println!("  sha256: <missing, verification will warn>"),
    }
    println!("  url: {}", asset.url);

    if args.dry_run {
        println!("\n(dry-run: skipping download)");
        return Ok(());
    }

    let dest = match &args.dest {
        Some(dest) => dest.clone(),
        None => default_dest(asset),
    };
    println!("\ndownloading -> {}", dest.display());
    download(&HttpClient::new().unwrap(), asset, &dest, DownloadOptions::default(), |done, total| {
        let percentage = if total > 0 { (done as f64 / total as f64) * 100.0 } else { 0.0 };
        let line = if total > 0 {
            format!("\r  {done}/{total} bytes ({percentage:.1}%)")
        } else {
            format!("\r  {done} bytes")
        };
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = write!(out, "{line}");
        let _ = out.flush();
    })
    .await?;
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    println!("\ninstalled {} ({size} bytes)", dest.display());
    Ok(())
}

async fn load_index(args: &Args) -> Result<MihomoIndex, Error> {
    let source = &args.source;
    if source.contains("://") {
        println!("fetching index from: {source}");
        let client = HttpClient::new().unwrap();
        return match &args.index_cache {
            Some(path) => {
                let cache = mihomo_versions::IndexCache { path: path.clone(), max_age: Duration::from_secs(600) };
                mihomo_versions::fetch_index_cached(&client, &[source], &cache).await
            }
            None => fetch_index(&client, &[source]).await,
        };
    }
    let path = resolve_project_path(Path::new(source));
    println!("loading local index: {}", path.display());
    if !path.is_file() {
        eprintln!("local index not found: {}", path.display());
        eprintln!("hint: for a remote index pass a full URL (https://...), otherwise pass a file path");
        std::process::exit(2);
    }
    let text = tokio::fs::read_to_string(path).await?;
    Ok(serde_json::from_str(&text)?)
}

/// Resolves a local index path. Absolute paths and existing cwd-relative paths
/// are used as-is; otherwise the path is resolved against the project root, so
/// `mihomo-releases.json` works no matter where the example is run from.
fn resolve_project_path(path: &Path) -> PathBuf {
    if path.is_absolute() || path.is_file() {
        path.to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
    }
}

fn default_dest(asset: &mihomo_versions::MihomoAsset) -> PathBuf {
    let mut name = asset.name.clone();
    for suffix in [".tar.gz", ".tar.xz", ".gz", ".zip"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.to_string();
            break;
        }
    }
    if name == "mihomo" && cfg!(windows) {
        name.push_str(".exe");
    }
    std::env::temp_dir().join(name)
}
