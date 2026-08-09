use std::time::Duration;

use mihomo_versions::Error;
use serde_json::json;
use tempfile::tempdir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

mod common;
use common::*;

#[tokio::test]
async fn fetch_index_and_pick_asset_by_name() {
    init_logger();
    let server = MockServer::start().await;
    let golden = std::fs::read_to_string("tests/fixture/golden-index.json").unwrap();
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(golden))
        .mount(&server)
        .await;

    let client = client();
    let index_url = format!("{}/mihomo-releases.json", server.uri());
    let index = mihomo_versions::fetch_index(&client, &[&index_url]).await.unwrap();
    assert_eq!(index.schema_version, 1);

    // By exact name, newest-first: the alpha asset resolves from the latest build.
    let latest = mihomo_versions::pick_asset_by_name(&index, "mihomo-linux-amd64-alpha.gz", None).unwrap();
    assert_eq!(latest.name, "mihomo-linux-amd64-alpha.gz");

    // Pinned by semver.
    let pinned = mihomo_versions::pick_asset_by_name(&index, "mihomo-linux-amd64-v1.19.9.gz", Some("1.19.9")).unwrap();
    assert_eq!(pinned.name, "mihomo-linux-amd64-v1.19.9.gz");

    // Unknown name -> AssetNotFound.
    assert!(matches!(mihomo_versions::pick_asset_by_name(&index, "nope.gz", None), Err(Error::AssetNotFound { .. })));

    // Channel filtering: alpha has the latest build, stable has the release.
    let alphas: Vec<&str> = mihomo_versions::sorted_versions_by_channel(&index, mihomo_versions::Channel::Alpha)
        .iter()
        .map(|v| v.tag.as_str())
        .collect();
    assert_eq!(alphas, vec!["Prerelease-Alpha"]);
    let stables: Vec<&str> = mihomo_versions::sorted_versions_by_channel(&index, mihomo_versions::Channel::Stable)
        .iter()
        .map(|v| v.tag.as_str())
        .collect();
    assert_eq!(stables, vec!["v1.19.9"]);

    // Old indexes without a channel field default to "stable".
    let legacy: mihomo_versions::MihomoIndex = serde_json::from_str(
        r#"{"schema_version":1,"generated_at":"2026-01-01T00:00:00Z","versions":[{"tag":"Prerelease-Alpha","assets":[]}]}"#,
    )
    .unwrap();
    assert_eq!(legacy.versions[0].channel, "stable");
}

#[tokio::test]
async fn fetch_index_fails_over_to_next_mirror() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/a")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_json_body()))
        .mount(&server)
        .await;

    let client = client();
    let urls = [format!("{}/a", server.uri()), format!("{}/b", server.uri())];
    let refs: Vec<&str> = urls.iter().map(|u| u.as_str()).collect();
    let index = mihomo_versions::fetch_index(&client, &refs).await.unwrap();
    assert_eq!(index.schema_version, 1);
    assert_eq!(requests_for(&server, "/b").await, 1);
}

#[tokio::test]
async fn fetch_index_errors_when_all_mirrors_fail() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/a")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

    let client = client();
    let urls = [format!("{}/a", server.uri()), format!("{}/a", server.uri())];
    let refs: Vec<&str> = urls.iter().map(|u| u.as_str()).collect();
    let result = mihomo_versions::fetch_index(&client, &refs).await;
    assert!(matches!(result, Err(Error::Http(404))));
}

#[tokio::test]
async fn fetch_index_serves_fresh_cache_without_network() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cache_path = dir.path().join("index.json");
    let cached = index_json_body().to_string();
    std::fs::write(&cache_path, &cached).unwrap();
    std::fs::write(dir.path().join("index.json.meta"), format!(r#"{{"fetched_at":"{}"}}"#, now_rfc3339())).unwrap();

    let client = client();
    let url = format!("{}/mihomo-releases.json", server.uri());
    let cache = mihomo_versions::IndexCache { path: cache_path.clone(), max_age: Duration::from_secs(3600) };
    let index = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await.unwrap();
    assert_eq!(index.generated_at, "2026-01-01T00:00:00Z");
    assert_eq!(requests_for(&server, "/mihomo-releases.json").await, 0, "fresh cache must not hit the network");
}

#[tokio::test]
async fn fetch_index_revalidates_with_304() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json"))
        .and(header("if-none-match", "abc"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cache_path = dir.path().join("index.json");
    let cached = index_json_body().to_string();
    std::fs::write(&cache_path, &cached).unwrap();
    std::fs::write(dir.path().join("index.json.meta"), format!(r#"{{"etag":"abc","fetched_at":"{OLD_RFC3339}"}}"#))
        .unwrap();

    let client = client();
    let url = format!("{}/mihomo-releases.json", server.uri());
    let cache = mihomo_versions::IndexCache { path: cache_path.clone(), max_age: Duration::from_secs(1) };
    let index = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await.unwrap();
    assert_eq!(index.generated_at, "2026-01-01T00:00:00Z");
    assert_eq!(requests_for(&server, "/mihomo-releases.json").await, 1);
}

#[tokio::test]
async fn fetch_index_updates_cache_on_200() {
    init_logger();
    let server = MockServer::start().await;
    let fresh_body = json!({
        "schema_version": 1,
        "generated_at": "2026-06-01T00:00:00Z",
        "versions": []
    });
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fresh_body).insert_header("etag", "new-etag"))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cache_path = dir.path().join("index.json");
    std::fs::write(&cache_path, index_json_body().to_string()).unwrap();
    std::fs::write(dir.path().join("index.json.meta"), format!(r#"{{"etag":"old","fetched_at":"{OLD_RFC3339}"}}"#))
        .unwrap();

    let client = client();
    let url = format!("{}/mihomo-releases.json", server.uri());
    let cache = mihomo_versions::IndexCache { path: cache_path.clone(), max_age: Duration::from_secs(1) };
    let index = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await.unwrap();
    assert_eq!(index.generated_at, "2026-06-01T00:00:00Z");

    let on_disk: serde_json::Value = serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(on_disk["generated_at"], "2026-06-01T00:00:00Z");
    let meta_text = std::fs::read_to_string(dir.path().join("index.json.meta")).unwrap();
    assert!(meta_text.contains("\"etag\":\"new-etag\""), "meta must store the new etag: {meta_text}");
}

#[tokio::test]
async fn fetch_index_falls_back_to_stale_cache() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cache_path = dir.path().join("index.json");
    let cached = index_json_body().to_string();
    std::fs::write(&cache_path, &cached).unwrap();
    std::fs::write(dir.path().join("index.json.meta"), format!(r#"{{"fetched_at":"{OLD_RFC3339}"}}"#)).unwrap();

    let client = client();
    let url = format!("{}/mihomo-releases.json", server.uri());
    let cache = mihomo_versions::IndexCache { path: cache_path.clone(), max_age: Duration::from_secs(1) };
    let index = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await.unwrap();
    assert_eq!(index.generated_at, "2026-01-01T00:00:00Z", "stale cache served on mirror failure");
}

#[tokio::test]
async fn fetch_index_errors_without_cache_on_mirror_failure() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = client();
    let url = format!("{}/mihomo-releases.json", server.uri());
    let cache = mihomo_versions::IndexCache {
        path: tempdir().unwrap().path().join("index.json"),
        max_age: Duration::from_secs(1),
    };
    let result = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await;
    assert!(matches!(result, Err(Error::Http(404))));
}

#[tokio::test]
async fn fetch_index_cached_missing_cache_file_falls_back_to_network() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_json_body()))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cache_path = dir.path().join("index.json");
    // The meta is fresh, but the cache file itself was never written: the
    // fetch must go to the network instead of failing on the missing file.
    std::fs::write(dir.path().join("index.json.meta"), format!(r#"{{"fetched_at":"{}"}}"#, now_rfc3339())).unwrap();

    let client = client();
    let url = format!("{}/mihomo-releases.json", server.uri());
    let cache = mihomo_versions::IndexCache { path: cache_path.clone(), max_age: Duration::from_secs(3600) };
    let index = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await.unwrap();
    assert_eq!(index.generated_at, "2026-01-01T00:00:00Z");
    assert_eq!(
        requests_for(&server, "/mihomo-releases.json").await,
        1,
        "a fresh meta without the cache file must still hit the network"
    );
    assert!(cache_path.is_file(), "the cache file should be written after a 200");
}

#[tokio::test]
async fn fetch_index_reads_gz_urls() {
    init_logger();
    let server = MockServer::start().await;
    let body = index_json_body().to_string();
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(gz_bytes(body.as_bytes())))
        .mount(&server)
        .await;

    let client = client();
    let url = format!("{}/mihomo-releases.json.gz", server.uri());
    let index = mihomo_versions::fetch_index(&client, &[&url]).await.unwrap();
    assert_eq!(index.generated_at, "2026-01-01T00:00:00Z");
}

#[tokio::test]
async fn fetch_index_cached_stores_plain_json_for_gz_url() {
    init_logger();
    let server = MockServer::start().await;
    let body = index_json_body().to_string();
    Mock::given(method("GET"))
        .and(path("/mihomo-releases.json.gz"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(gz_bytes(body.as_bytes())).insert_header("etag", "gz-etag"),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cache_path = dir.path().join("index.json");
    let client = client();
    let url = format!("{}/mihomo-releases.json.gz", server.uri());
    let cache = mihomo_versions::IndexCache { path: cache_path.clone(), max_age: Duration::from_secs(3600) };
    let index = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await.unwrap();
    assert_eq!(index.generated_at, "2026-01-01T00:00:00Z");
    // The cache file must be plain (decompressed) JSON, not the gzip bytes.
    let cached: serde_json::Value = serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(cached["generated_at"], "2026-01-01T00:00:00Z");
    let meta_text = std::fs::read_to_string(dir.path().join("index.json.meta")).unwrap();
    assert!(meta_text.contains("\"etag\":\"gz-etag\""));

    // A fresh cache is served from the plain JSON file without decompression.
    let index2 = mihomo_versions::fetch_index_cached(&client, &[&url], &cache).await.unwrap();
    assert_eq!(index2.generated_at, "2026-01-01T00:00:00Z");
}
