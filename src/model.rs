use serde::{Deserialize, Serialize};

/// Current index schema version understood by this library.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MihomoIndex {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub generated_at: String,
    pub versions: Vec<MihomoVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MihomoVersion {
    /// Normalized semver, or `None` for non-semver tags (e.g. `Prerelease-Alpha`,
    /// which represents the latest build).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semver: Option<String>,
    /// Original Git tag.
    pub tag: String,
    #[serde(default)]
    pub prerelease: bool,
    /// Distribution channel, classified at sync time: `stable` / `alpha` /
    /// `nightly`. Old indexes without the field default to `stable`.
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Release creation time (RFC3339, from the GitHub API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Release last-update time (RFC3339, from the GitHub API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub assets: Vec<MihomoAsset>,
}

fn default_channel() -> String {
    "stable".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MihomoAsset {
    pub name: String,
    /// Canonical platform identifier, e.g. `darwin-arm64`.
    pub platform: String,
    /// How the download is processed: `gz`/`zip` are decompressed, `raw` is
    /// copied as-is.
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// SHA-256 of the archive, when the source release published a checksum.
    /// `None` means the digest is unavailable and verification is skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Asset creation time (RFC3339, from the GitHub API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Asset last-update time (RFC3339, from the GitHub API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub url: String,
}

/// Normalizes a Git tag into a semver string when possible.
/// Returns `None` for tags that are not version tags (e.g. `Prerelease-Alpha`).
pub fn normalize_tag(tag: &str) -> Option<String> {
    let trimmed = tag.trim_start_matches(['v', 'V']);
    semver::Version::parse(trimmed).ok().map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_version_tags() {
        assert_eq!(normalize_tag("v1.19.9").as_deref(), Some("1.19.9"));
        assert_eq!(normalize_tag("V1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(normalize_tag("1.10.0").as_deref(), Some("1.10.0"));
        assert_eq!(normalize_tag("v1.2.3-beta.1").as_deref(), Some("1.2.3-beta.1"));
    }

    #[test]
    fn rejects_non_version_tags() {
        assert_eq!(normalize_tag("Prerelease-Alpha"), None);
        assert_eq!(normalize_tag("main"), None);
        assert_eq!(normalize_tag(""), None);
    }

    #[test]
    fn roundtrips_serde_with_optional_fields() {
        let json = r#"{
            "schema_version": 1,
            "generated_at": "2026-08-02T00:00:00Z",
            "versions": [
                {
                    "tag": "Prerelease-Alpha",
                    "assets": []
                },
                {
                    "semver": "1.19.9",
                    "tag": "v1.19.9",
                    "assets": []
                }
            ]
        }"#;
        let index: MihomoIndex = serde_json::from_str(json).unwrap();
        assert!(index.source.is_none());
        assert_eq!(index.versions.len(), 2);
        assert_eq!(index.versions[0].semver, None);
        assert!(!index.versions[0].prerelease);
        assert_eq!(index.versions[1].semver.as_deref(), Some("1.19.9"));
    }

    #[test]
    fn ignores_unknown_fields() {
        let json = r#"{"schema_version":1,"generated_at":"x","future_field":true,"versions":[]}"#;
        let index: MihomoIndex = serde_json::from_str(json).unwrap();
        assert!(index.versions.is_empty());
    }
}
