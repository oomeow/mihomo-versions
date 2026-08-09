//! Smoke tests against the real GitHub Releases API dump captured at the repo
//! root (`github-release.json`, ~10.8MB / 61 releases / 4686 assets).
//!
//! These run only when explicitly enabled (`cargo test -- --ignored`) because
//! they depend on the committed dump matching the snapshot counts.

use std::{collections::HashMap, path::Path};

use mihomo_versions::{
    classify::{classify, mihomo_config},
    normalize_tag,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    #[serde(default)]
    size: u64,
    browser_download_url: String,
}

const EXPECTED_TOTAL_ASSETS: usize = 4686;
const DUMP_PATH: &str = "github-release.json";

fn load_dump() -> Vec<GhRelease> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(DUMP_PATH);
    let bytes = std::fs::read(&path).expect("missing real dump; run from repo root");
    serde_json::from_slice(&bytes).expect("dump is not valid JSON")
}

/// Initializes the logger once per test process so `log` output is visible
/// under `cargo test -- --nocapture` (defaults to `debug`, overridable via
/// `RUST_LOG`).
fn init_logger() {
    use std::sync::OnceLock;
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).try_init();
    });
}

#[test]
#[ignore = "requires the committed 10.8MB dump; run locally"]
fn classifies_every_asset_in_real_dump_without_panicking() {
    init_logger();
    let releases = load_dump();
    let mut total = 0usize;
    let mut kept = 0usize;
    let mut by_platform: HashMap<String, usize> = HashMap::new();

    for release in &releases {
        for asset in &release.assets {
            total += 1;
            if let Some(indexed) = classify(&mihomo_config(), &asset.name, &asset.browser_download_url, asset.size) {
                kept += 1;
                *by_platform.entry(indexed.platform).or_default() += 1;
            }
        }
    }

    assert_eq!(total, EXPECTED_TOTAL_ASSETS, "dump changed; update the snapshot count");
    assert!(kept > 0);
    // Every supported platform must actually appear in the real dump.
    for platform in mihomo_versions::Platform::ALL {
        assert!(by_platform.get(platform.as_str()).copied().unwrap_or(0) > 0, "missing {} in dump", platform.as_str());
    }
    eprintln!("classified {kept}/{total} assets: {by_platform:?}");
}

#[test]
#[ignore = "requires the committed 10.8MB dump; run locally"]
fn latest_build_tag_is_preserved_as_null_semver() {
    init_logger();
    let releases = load_dump();
    let mut saw_latest_build = false;
    for release in &releases {
        if release.tag_name == "Prerelease-Alpha" {
            saw_latest_build = true;
            assert_eq!(normalize_tag(&release.tag_name), None);
        } else {
            assert!(
                normalize_tag(&release.tag_name).is_some(),
                "tag {} should normalize or be handled as latest build",
                release.tag_name
            );
        }
    }
    assert!(saw_latest_build, "Prerelease-Alpha should exist in the dump");
}
