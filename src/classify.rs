use serde::{Deserialize, Serialize};

use crate::{error::Error, model::MihomoAsset, platform::Platform};

/// Per-repository asset classification rules.
///
/// The sync tool must not hard-code any repository's naming conventions, since
/// different GitHub repos name their binary assets differently. Instead, the
/// mapping from asset name -> platform/format is supplied as data.
/// `mihomo_config()` bundles the defaults for MetaCubeX/mihomo; other
/// repositories pass their own rules via `--classifier <path>`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClassifierConfig {
    /// File extensions kept in the index (e.g. `gz`, `zip`), matched against
    /// the last extension of the asset name. An empty list accepts every type
    /// (including raw executables with no extension). `keep_formats` is accepted
    /// as a legacy alias.
    #[serde(default = "default_keep_extensions", alias = "keep_formats")]
    pub keep_extensions: Vec<String>,
    /// Exact asset names to skip (e.g. auxiliary files).
    #[serde(default)]
    pub exclude_names: Vec<String>,
    /// Platform rules, matched in order.
    pub platforms: Vec<PlatformRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformRule {
    /// Canonical platform identifier written to the index (e.g. `darwin-arm64`).
    pub name: String,
    /// Substrings that identify this platform in an asset name.
    pub patterns: Vec<String>,
}

fn default_keep_extensions() -> Vec<String> {
    vec!["gz".to_string(), "zip".to_string(), "zst".to_string()]
}

impl ClassifierConfig {
    /// Ensures every `platforms[].name` is one of the `Platform` enum values the
    /// client can parse. Configurations naming other platforms would produce an
    /// index the client cannot consume, so they are rejected up front.
    pub fn validate(&self) -> Result<(), Error> {
        let supported: Vec<&str> = Platform::ALL.iter().map(|p| p.as_str()).collect();
        let mut invalid: Vec<&str> = self
            .platforms
            .iter()
            .map(|rule| rule.name.as_str())
            .filter(|name| crate::platform::parse_platform(name).is_err())
            .collect();
        invalid.sort_unstable();
        invalid.dedup();
        if invalid.is_empty() {
            Ok(())
        } else {
            Err(Error::InvalidSchema(format!(
                "unsupported platform name(s) in classifier config: {}; must be one of: {}",
                invalid.join(", "),
                supported.join(", ")
            )))
        }
    }
}

/// Classification rules for the default MetaCubeX/mihomo repository.
///
/// The rules live in `classify/mihomo.json` (the single source of truth,
/// also serving as the template for other repositories' rules). The config is
/// embedded at compile time so the sync binary works without the file present.
pub fn mihomo_config() -> ClassifierConfig {
    serde_json::from_str(include_str!("../classifier/mihomo.json"))
        .expect("bundled classifier config must be valid JSON")
}

/// Classifies a release asset name into an index asset using the given rules,
/// returning `None` to skip the asset.
pub fn classify(config: &ClassifierConfig, name: &str, url: &str, size: u64) -> Option<MihomoAsset> {
    if config.exclude_names.iter().any(|n| n == name) {
        log::debug!("classify: skipping excluded asset {name}");
        return None;
    }
    // An empty keep_extensions accepts every type (including raw executables
    // with no extension); otherwise only listed extensions are kept. Compound
    // archives match on their last extension (e.g. `gz` of `.tar.gz`), so a
    // `["gz", "zip"]` keep_extensions accepts `.tar.gz` too.
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if !config.keep_extensions.is_empty() && !config.keep_extensions.iter().any(|f| f == ext) {
        log::debug!("classify: skipping {name} (extension {ext} not kept)");
        return None;
    }
    let platform = match config.platforms.iter().find(|rule| rule.patterns.iter().any(|p| name.contains(p.as_str()))) {
        Some(rule) => rule,
        None => {
            log::debug!("classify: skipping {name} (no platform rule matches)");
            return None;
        }
    };
    let format = classify_format(name, ext);
    log::debug!("classify: {name} -> {} (format={format})", platform.name);

    Some(MihomoAsset {
        name: name.to_string(),
        platform: platform.name.clone(),
        format,
        size: Some(size),
        sha256: None,
        created_at: None,
        updated_at: None,
        url: url.to_string(),
    })
}

/// Maps an asset name to the processing format written into the index:
/// `tar.gz` (gunzip + untar), `gz`/`zip`/`zst` (decompress), everything else
/// `raw`.
fn classify_format(name: &str, ext: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") {
        "tar.gz".to_string()
    } else if ext == "gz" || ext == "zip" || ext == "zst" {
        ext.to_string()
    } else {
        "raw".to_string()
    }
}

/// Normalizes the GitHub API `digest` field (e.g. `sha256:<hex>`) into a
/// lowercase hex string, returning `None` for missing or malformed values.
pub fn normalize_digest(raw: &str) -> Option<String> {
    let hex = raw.strip_prefix("sha256:").unwrap_or(raw).trim();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) { Some(hex.to_ascii_lowercase()) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClassifierConfig {
        mihomo_config()
    }

    #[test]
    fn classifies_plain_build() {
        let asset = classify(
            &cfg(),
            "mihomo-darwin-arm64-v1.19.9.gz",
            "https://example.com/mihomo-darwin-arm64-v1.19.9.gz",
            123,
        )
        .unwrap();
        assert_eq!(asset.platform, "darwin-aarch64");
        assert_eq!(asset.format, "gz");
        assert_eq!(asset.sha256, None);
    }

    #[test]
    fn classifies_variant_build() {
        let asset = classify(&cfg(), "mihomo-linux-amd64-compatible-v1.19.9.gz", "u", 1).unwrap();
        assert_eq!(asset.platform, "linux-x86_64");
        assert_eq!(asset.format, "gz");
    }

    #[test]
    fn classifies_legacy_prefix() {
        let asset = classify(&cfg(), "Clash.Meta-darwin-amd64-v1.10.0.gz", "u", 1).unwrap();
        assert_eq!(asset.platform, "darwin-x86_64");
    }

    #[test]
    fn classifies_windows_zip() {
        let asset = classify(&cfg(), "mihomo-windows-amd64-v1.19.9.zip", "u", 1).unwrap();
        assert_eq!(asset.platform, "windows-x86_64");
        assert_eq!(asset.format, "zip");
    }

    #[test]
    fn classifies_tar_gz_compound_format() {
        let asset = classify(&cfg(), "mihomo-darwin-arm64-v1.19.9.tar.gz", "u", 1).unwrap();
        assert_eq!(asset.platform, "darwin-aarch64");
        assert_eq!(asset.format, "tar.gz");
        // A plain .gz stays gz.
        assert_eq!(classify(&cfg(), "mihomo-darwin-arm64-v1.19.9.gz", "u", 1).unwrap().format, "gz");
    }

    #[test]
    fn classifies_meow_rs_tar_gz_asset() {
        let config: ClassifierConfig = serde_json::from_str(include_str!("../classifier/meow-rs.json")).unwrap();
        config.validate().unwrap();
        let asset = classify(&config, "meow-v0.19.0-aarch64-apple-darwin.tar.gz", "u", 1).unwrap();
        assert_eq!(asset.platform, "darwin-aarch64");
        assert_eq!(asset.format, "tar.gz");
    }

    #[test]
    fn classifies_zst_assets() {
        let asset = classify(&cfg(), "mihomo-linux-amd64-v1.19.9.zst", "u", 1).unwrap();
        assert_eq!(asset.platform, "linux-x86_64");
        assert_eq!(asset.format, "zst");
    }

    #[test]
    fn matches_architecture_aliases_without_cross_matching() {
        // Alias spellings of the canonical arch match the right platform.
        assert_eq!(classify(&cfg(), "mihomo-darwin-aarch64-v1.19.9.gz", "u", 1).unwrap().platform, "darwin-aarch64");
        assert_eq!(classify(&cfg(), "mihomo-linux-x86_64-v1.19.9.gz", "u", 1).unwrap().platform, "linux-x86_64");
        // The OS check keeps aliases from cross-matching: linux-arm64 must
        // not be claimed by the darwin-arm64 rule just because of "arm64".
        assert_eq!(classify(&cfg(), "mihomo-linux-arm64-v1.19.9.gz", "u", 1).unwrap().platform, "linux-aarch64");
    }

    #[test]
    fn skips_unsupported_platforms_and_formats() {
        assert!(classify(&cfg(), "mihomo-plan9-amd64-v1.19.9.gz", "u", 1).is_none());
        assert!(classify(&cfg(), "mihomo-linux-ppc64-v1.19.9.gz", "u", 1).is_none());
        assert!(classify(&cfg(), "mihomo-solaris-amd64-v1.19.9.gz", "u", 1).is_none());
        assert!(classify(&cfg(), "mihomo-linux-amd64-v1.19.9.deb", "u", 1).is_none());
        assert!(classify(&cfg(), "mihomo-linux-amd64-v1.19.9.rpm", "u", 1).is_none());
        assert!(classify(&cfg(), "checksums.txt", "u", 1).is_none());
    }

    #[test]
    fn custom_config_classifies_differently_named_repo() {
        let config = ClassifierConfig {
            keep_extensions: vec!["gz".into()],
            exclude_names: vec!["README".into()],
            platforms: vec![PlatformRule {
                name: "darwin-aarch64".into(),
                patterns: vec!["aarch64-apple-darwin".into(), "macos_arm64".into()],
            }],
        };
        config.validate().unwrap();
        assert!(classify(&config, "README", "u", 1).is_none());
        let via_alt_pattern = classify(&config, "app-aarch64-apple-darwin.gz", "u", 1).unwrap();
        assert_eq!(via_alt_pattern.platform, "darwin-aarch64");
        let plain = classify(&config, "app-macos_arm64.gz", "u", 1).unwrap();
        assert_eq!(plain.platform, "darwin-aarch64");
        // format not kept -> skipped
        assert!(classify(&config, "app-macos_arm64.zip", "u", 1).is_none());
    }

    #[test]
    fn empty_keep_extensions_accepts_all_types() {
        let config = ClassifierConfig {
            keep_extensions: vec![],
            exclude_names: vec!["README".into()],
            platforms: vec![PlatformRule { name: "linux-x86_64".into(), patterns: vec!["linux-x86_64".into()] }],
        };
        // Raw executable with no extension.
        let raw = classify(&config, "mihomo-linux-x86_64", "u", 1).unwrap();
        assert_eq!(raw.format, "raw");
        // Archives keep their format.
        assert_eq!(classify(&config, "mihomo-linux-x86_64.gz", "u", 1).unwrap().format, "gz");
        assert_eq!(classify(&config, "mihomo-linux-x86_64.zip", "u", 1).unwrap().format, "zip");
        // Other extensions are accepted too (raw copy).
        assert_eq!(classify(&config, "mihomo-linux-x86_64.deb", "u", 1).unwrap().format, "raw");
        // exclude_names still applies.
        assert!(classify(&config, "README", "u", 1).is_none());
    }

    #[test]
    fn deserializes_legacy_keep_formats_alias() {
        let config: ClassifierConfig = serde_json::from_str(r#"{"keep_formats": ["gz"], "platforms": []}"#).unwrap();
        assert_eq!(config.keep_extensions, vec!["gz".to_string()]);
    }

    #[test]
    fn validate_accepts_supported_platforms() {
        mihomo_config().validate().unwrap();
    }

    #[test]
    fn validate_rejects_unknown_platform_names() {
        let config = ClassifierConfig {
            platforms: vec![PlatformRule { name: "solaris-sparc".into(), patterns: vec!["solaris".into()] }],
            ..ClassifierConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("solaris-sparc"));
        assert!(err.to_string().contains("darwin-aarch64"));
    }

    #[test]
    fn normalizes_github_digest_field() {
        let hex = "006fe93f7ec73e29af8f549b6f4a3e2db704cca6dd1cfb33a742fce4133dff85";
        assert_eq!(normalize_digest(&format!("sha256:{hex}")).as_deref(), Some(hex));
        assert_eq!(normalize_digest(hex).as_deref(), Some(hex));
        assert_eq!(normalize_digest("sha256:ABC"), None);
        assert_eq!(normalize_digest(""), None);
        assert_eq!(normalize_digest("sha256:"), None);
    }
}
