use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

mod common;
use common::*;

#[tokio::test]
async fn query_api_filters_lists_assets_and_diffs() {
    let server = MockServer::start().await;
    let index_json = json!({
        "schema_version": 1,
        "generated_at": "2026-01-01T00:00:00Z",
        "versions": [
            {
                "semver": "1.0.0",
                "tag": "v1.0.0",
                "prerelease": false,
                "channel": "stable",
                "published_at": "2026-01-01T00:00:00Z",
                "assets": [
                    {"name": "mihomo-darwin-arm64-v1.0.0.gz", "platform": "darwin-aarch64", "format": "gz", "size": null, "sha256": null, "url": "https://example.com/a"},
                    {"name": "mihomo-linux-amd64-v1.0.0.gz", "platform": "linux-x86_64", "format": "gz", "size": null, "sha256": null, "url": "https://example.com/b"}
                ]
            },
            {
                "semver": null,
                "tag": "Prerelease-Alpha",
                "prerelease": true,
                "channel": "alpha",
                "published_at": "2026-01-01T00:00:00Z",
                "assets": [{"name": "mihomo-linux-amd64-alpha.gz", "platform": "linux-x86_64", "format": "gz", "size": null, "sha256": null, "url": "https://example.com/c"}]
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/query-index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_json))
        .mount(&server)
        .await;

    let client = client();
    let url = format!("{}/query-index.json", server.uri());
    let index = mihomo_versions::fetch_index(&client, &[&url]).await.unwrap();

    use mihomo_versions::{Channel, Platform, VersionFilter};
    let stables = mihomo_versions::list_versions(
        &index,
        Some(&VersionFilter { channel: Some(Channel::Stable), ..Default::default() }),
    );
    assert_eq!(stables.len(), 1);
    assert_eq!(stables[0].tag, "v1.0.0");

    let alphas =
        mihomo_versions::list_versions(&index, Some(&VersionFilter { prerelease: Some(true), ..Default::default() }));
    assert_eq!(alphas.len(), 1);

    let linux = mihomo_versions::assets_for_platform(&index, Platform::LinuxX86_64, None);
    assert_eq!(linux.len(), 2);
    assert_eq!(linux[0].tag, "Prerelease-Alpha");
    assert_eq!(linux[0].assets.len(), 1);
    assert_eq!(linux[0].assets[0].name, "mihomo-linux-amd64-alpha.gz");
    assert_eq!(linux[1].tag, "v1.0.0");
    assert_eq!(linux[1].assets.len(), 1);
    assert_eq!(linux[1].assets[0].name, "mihomo-linux-amd64-v1.0.0.gz");

    let filtered = mihomo_versions::assets_for_platform(
        &index,
        Platform::LinuxX86_64,
        Some(&VersionFilter { channel: Some(Channel::Stable), ..Default::default() }),
    );
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].tag, "v1.0.0");
}
