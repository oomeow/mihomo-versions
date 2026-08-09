use std::io::Read;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use tempfile::tempdir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

mod common;
use common::*;

#[tokio::test]
async fn sync_rejects_classifier_with_unknown_platform() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let classifier = dir.path().join("bad.json");
    std::fs::write(&classifier, r#"{"platforms":[{"name":"solaris-sparc","patterns":["solaris"]}]}"#).unwrap();

    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .args([
            "--out",
            dir.path().join("index.json").to_str().unwrap(),
            "--repo",
            "MetaCubeX/mihomo",
            "--api-base",
            &server.uri(),
            "--classifier",
            classifier.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported platform name"));
}

#[tokio::test]
async fn sync_honors_custom_classifier_config() {
    init_logger();
    let server = MockServer::start().await;
    let digest = "a".repeat(64);

    mount_releases_page(
        &server,
        json!([release(
            "v1.19.9",
            false,
            vec![
                gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/d.gz", Some(&digest)),
                gh_asset("mihomo-linux-amd64-v1.19.9.gz", "https://example.com/l.gz", None),
                gh_asset("mihomo-darwin-arm64-compatible-v1.19.9.gz", "https://example.com/c.gz", None),
            ],
        )]),
    )
    .await;

    // A config for a hypothetical repo that only knows darwin-arm64.
    let dir = tempdir().unwrap();
    let classifier = dir.path().join("classifier.json");
    std::fs::write(
        &classifier,
        r#"{
            "keep_extensions": ["gz"],
            "platforms": [{ "name": "darwin-aarch64", "patterns": ["darwin-arm64"] }]
        }"#,
    )
    .unwrap();

    let out = dir.path().join("index.json");
    // Run from the temp dir with a relative classifier path and an absolute
    // --out, proving relative CLI paths resolve correctly.
    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "--out",
            out.to_str().unwrap(),
            "--repo",
            "MetaCubeX/mihomo",
            "--api-base",
            &server.uri(),
            "--classifier",
            "classifier.json",
        ])
        .assert()
        .success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let assets = index["versions"][0]["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 2, "linux-amd64 should be dropped by the custom rules");
    assert_eq!(assets[0]["platform"], "darwin-aarch64");
    assert_eq!(assets[1]["platform"], "darwin-aarch64");
}

#[tokio::test]
async fn sync_builds_compact_index() {
    init_logger();
    let server = MockServer::start().await;
    let bin_url = format!("{}/bin.gz", server.uri());
    let digest = "a".repeat(64);
    let variant_digest = "b".repeat(64);

    mount_releases_page(
        &server,
        json!([
            release("Prerelease-Alpha", true, vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", &bin_url, None)]),
            release(
                "v1.19.9",
                false,
                vec![
                    gh_asset("mihomo-darwin-arm64-v1.19.9.gz", &bin_url, Some(&digest)),
                    gh_asset(
                        "mihomo-darwin-amd64-compatible-v1.19.9.gz",
                        "https://example.com/c.gz",
                        Some(&variant_digest)
                    ),
                    gh_asset("mihomo-linux-amd64-v1.19.9.deb", "https://example.com/x.deb", None),
                ],
            ),
            release("v1.19.8", false, vec![]),
        ]),
    )
    .await;

    Mock::given(method("GET"))
        .and(path("/bin.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bin"))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["schema_version"], 1);
    assert_eq!(index["source"]["owner"], "MetaCubeX");
    assert_eq!(index["source"]["repo"], "mihomo");
    assert!(index["generated_at"].as_str().unwrap().starts_with("20"));
    assert_eq!(index["versions"].as_array().unwrap().len(), 3);

    let alpha = &index["versions"][0];
    assert_eq!(alpha["tag"], "Prerelease-Alpha");
    assert_eq!(alpha["semver"], serde_json::Value::Null);
    assert_eq!(alpha["prerelease"], true);
    assert_eq!(alpha["channel"], "alpha");

    let v = &index["versions"][1];
    assert_eq!(v["semver"], "1.19.9");
    assert_eq!(v["tag"], "v1.19.9");
    assert_eq!(v["channel"], "stable");
    let assets = v["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 2, "deb should be dropped");
    assert_eq!(assets[0]["platform"], "darwin-aarch64");
    assert_eq!(assets[0]["format"], "gz");
    assert_eq!(assets[0]["sha256"], digest);
    assert_eq!(assets[1]["platform"], "darwin-x86_64");
    assert_eq!(assets[1]["sha256"], variant_digest);
}

#[tokio::test]
async fn sync_refreshes_full_release_list_each_run() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    mount_releases_page(&server, json!([release("v1.19.8", false, vec![])])).await;
    run_sync(&server.uri(), &out).success();

    // The index is rebuilt from the API on every run, regardless of what was
    // generated before: a removed release must not linger.
    server.reset().await;
    mount_releases_page(&server, json!([release("v1.19.9", false, vec![]), release("v1.19.8", false, vec![])])).await;
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let tags: Vec<&str> = index["versions"].as_array().unwrap().iter().map(|v| v["tag"].as_str().unwrap()).collect();
    assert_eq!(tags, vec!["v1.19.9", "v1.19.8"]);
}

#[tokio::test]
async fn sync_uses_asset_digest() {
    init_logger();
    let server = MockServer::start().await;
    let hex = "006fe93f7ec73e29af8f549b6f4a3e2db704cca6dd1cfb33a742fce4133dff85";

    mount_releases_page(
        &server,
        json!([release(
            "v1.19.9",
            false,
            vec![
                gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(hex)),
                gh_asset("mihomo-linux-amd64-v1.19.9.gz", "https://example.com/b.gz", None),
            ],
        )]),
    )
    .await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let assets = index["versions"][0]["assets"].as_array().unwrap();
    assert_eq!(assets[0]["sha256"], hex);
    assert_eq!(assets[1]["sha256"], serde_json::Value::Null);
}

#[tokio::test]
async fn sync_refreshes_prerelease_builds_and_keeps_stables() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    let first_digest = "a".repeat(64);
    let second_digest = "b".repeat(64);

    mount_releases_page(
        &server,
        json!([
            release(
                "Prerelease-Alpha",
                true,
                vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&first_digest))],
            ),
            release("v1.19.9", false, vec![])
        ]),
    )
    .await;
    run_sync(&server.uri(), &out).success();

    // The alpha is rebuilt under the same tag: refresh it, keep the stable.
    // Incremental sync keys on updated_at, so the mock must bump it to
    // trigger reprocessing.
    server.reset().await;
    let mut alpha = release(
        "Prerelease-Alpha",
        true,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&second_digest))],
    );
    alpha["published_at"] = json!("2026-08-02T00:00:00Z");
    alpha["updated_at"] = json!("2026-08-02T00:00:00Z");
    mount_releases_page(&server, json!([alpha, release("v1.19.9", false, vec![])])).await;
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let versions = index["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2, "same-tag prerelease must not duplicate");

    let alpha = &versions[0];
    assert_eq!(alpha["tag"], "Prerelease-Alpha");
    assert_eq!(alpha["published_at"], "2026-08-02T00:00:00Z");
    assert_eq!(alpha["assets"][0]["sha256"], second_digest);
    assert_eq!(versions[1]["tag"], "v1.19.9");
}

#[tokio::test]
async fn rate_limit_surfaces_clear_error() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/MetaCubeX/mihomo/releases"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    run_sync(&server.uri(), &dir.path().join("index.json")).failure().stderr(predicate::str::contains("rate limit"));
}

#[tokio::test]
async fn sync_writes_compact_json_with_flag() {
    let server = MockServer::start().await;
    mount_releases_page(&server, json!([release("v1.19.9", false, vec![])])).await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .args(["--out", out.to_str().unwrap(), "--repo", "MetaCubeX/mihomo", "--api-base", &server.uri(), "--compact"])
        .assert()
        .success();

    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!text.contains('\n'), "compact output must be a single line, got: {}", text.lines().count());
    let _: serde_json::Value = serde_json::from_str(&text).unwrap();
}

#[tokio::test]
async fn sync_writes_gz_index() {
    let server = MockServer::start().await;
    mount_releases_page(&server, json!([release("v1.19.9", false, vec![])])).await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .args(["--out", out.to_str().unwrap(), "--repo", "MetaCubeX/mihomo", "--api-base", &server.uri(), "--gz"])
        .assert()
        .success();

    let gz_path = dir.path().join("index.json.gz");
    assert!(gz_path.is_file(), "--gz must append .gz to the output path");
    let mut decoder = flate2::read::GzDecoder::new(std::fs::File::open(&gz_path).unwrap());
    let mut text = String::new();
    decoder.read_to_string(&mut text).unwrap();
    let index: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(index["source"]["repo"], "mihomo");
}

#[test]
fn print_classifier_outputs_valid_config() {
    let output = Command::cargo_bin("mihomo-versions-sync").unwrap().arg("--print-classifier").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let config: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(config["platforms"].is_array());
    assert_eq!(config["platforms"].as_array().unwrap().len(), mihomo_versions::Platform::ALL.len());
    assert!(config["keep_extensions"].as_array().unwrap().contains(&json!("zst")));
}

#[tokio::test]
async fn sync_populates_created_and_updated_at() {
    init_logger();
    let server = MockServer::start().await;
    let mut rel =
        release("v1.19.9", false, vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", None)]);
    rel["created_at"] = json!("2026-07-29T00:00:00Z");
    rel["updated_at"] = json!("2026-07-30T00:00:00Z");
    rel["assets"][0]["created_at"] = json!("2026-07-29T01:00:00Z");
    rel["assets"][0]["updated_at"] = json!("2026-07-30T01:00:00Z");
    mount_releases_page(&server, json!([rel])).await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["versions"][0]["created_at"], "2026-07-29T00:00:00Z");
    assert_eq!(index["versions"][0]["updated_at"], "2026-07-30T00:00:00Z");
    assert_eq!(index["versions"][0]["assets"][0]["created_at"], "2026-07-29T01:00:00Z");
    assert_eq!(index["versions"][0]["assets"][0]["updated_at"], "2026-07-30T01:00:00Z");
}

#[tokio::test]
async fn incremental_reuses_unchanged_and_reprocesses_changed_releases() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    // First run: two releases, both with updated_at set.
    let mut stable = release(
        "v1.19.9",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))],
    );
    stable["updated_at"] = json!("2026-07-30T00:00:00Z");
    let mut alpha = release(
        "Prerelease-Alpha",
        true,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"b".repeat(64)))],
    );
    alpha["updated_at"] = json!("2026-08-01T00:00:00Z");
    mount_releases_page(&server, json!([alpha.clone(), stable.clone()])).await;
    run_sync(&server.uri(), &out).success();

    // Second run: same stable (unchanged), alpha with a new digest and new
    // updated_at. The stable must be reused as-is; alpha reprocessed.
    server.reset().await;
    let mut alpha_new = alpha.clone();
    alpha_new["updated_at"] = json!("2026-08-02T00:00:00Z");
    alpha_new["assets"] =
        json!([gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"c".repeat(64)))]);
    mount_releases_page(&server, json!([alpha_new, stable.clone()])).await;
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let versions = index["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    // Stable reused: still carries the first digest (a...) — untouched.
    let stable_idx = versions.iter().find(|v| v["tag"] == "v1.19.9").unwrap();
    assert_eq!(stable_idx["assets"][0]["sha256"], "a".repeat(64));
    // Alpha reprocessed: new digest (c...).
    let alpha_idx = versions.iter().find(|v| v["tag"] == "Prerelease-Alpha").unwrap();
    assert_eq!(alpha_idx["assets"][0]["sha256"], "c".repeat(64));
}

#[tokio::test]
async fn asset_updated_at_change_triggers_reprocess_even_when_release_unchanged() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    let mut rel = release(
        "v1.19.9",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))],
    );
    rel["updated_at"] = json!("2026-07-30T00:00:00Z");
    rel["assets"][0]["updated_at"] = json!("2026-07-30T00:00:00Z");
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success();

    // Same release updated_at, but the asset was re-uploaded (new digest +
    // new asset updated_at): must reprocess.
    server.reset().await;
    let mut rel_new = rel.clone();
    rel_new["assets"] =
        json!([gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"e".repeat(64)))]);
    rel_new["assets"][0]["updated_at"] = json!("2026-08-01T00:00:00Z");
    mount_releases_page(&server, json!([rel_new])).await;
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["versions"][0]["assets"][0]["sha256"], "e".repeat(64));
}

#[tokio::test]
async fn incremental_reprocesses_when_version_derived_fields_stale() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    // First run: one stable release with the same release/asset timestamps.
    let mut rel = release(
        "v1.19.9",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))],
    );
    rel["updated_at"] = json!("2026-07-30T00:00:00Z");
    rel["assets"][0]["updated_at"] = json!("2026-07-30T00:00:00Z");
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success();
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["versions"][0]["channel"], "stable");

    // Simulate a version-classification rule upgrade between runs: the
    // previous index carries a stale derived field (channel) that no longer
    // matches what the current rules would produce, while the GitHub-side
    // release/asset timestamps are unchanged.
    let mut stale = index.clone();
    stale["versions"][0]["channel"] = json!("alpha");
    std::fs::write(&out, serde_json::to_vec(&stale).unwrap()).unwrap();

    server.reset().await;
    mount_releases_page(&server, json!([rel])).await;
    run_sync(&server.uri(), &out).success();

    // The stale channel must not be reused; the entry is reprocessed and the
    // derived field corrected to `stable`.
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["versions"][0]["channel"], "stable");
}

#[tokio::test]
async fn emit_gz_writes_both_plain_and_gzip() {
    init_logger();
    let server = MockServer::start().await;
    mount_releases_page(&server, json!([release("v1.19.9", false, vec![])])).await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    run_sync_args(&server.uri(), &out, &["--emit-gz"]).success();

    assert!(out.is_file(), "--emit-gz must write the plain JSON index");
    let gz_path = dir.path().join("index.json.gz");
    assert!(gz_path.is_file(), "--emit-gz must write the gzip copy");
    let plain: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let mut decoder = flate2::read::GzDecoder::new(std::fs::File::open(&gz_path).unwrap());
    let mut text = String::new();
    decoder.read_to_string(&mut text).unwrap();
    let gz: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(plain, gz, "plain and gzip copies must carry the same index");
}

#[tokio::test]
async fn incremental_drops_releases_deleted_on_github() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    // First run: two releases, both with updated_at set.
    let mut keep = release(
        "v1.19.9",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))],
    );
    keep["updated_at"] = json!("2026-07-30T00:00:00Z");
    let mut gone = release(
        "v0.9.0",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v0.9.0.gz", "https://example.com/b.gz", Some(&"b".repeat(64)))],
    );
    gone["updated_at"] = json!("2026-06-01T00:00:00Z");
    mount_releases_page(&server, json!([keep.clone(), gone.clone()])).await;
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["versions"].as_array().unwrap().len(), 2);

    // Second run: v0.9.0 was deleted on GitHub; it must vanish from the index.
    server.reset().await;
    mount_releases_page(&server, json!([keep.clone()])).await;
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let versions = index["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1, "deleted release must be dropped");
    assert_eq!(versions[0]["tag"], "v1.19.9");
}

#[tokio::test]
async fn incremental_reuses_release_even_with_dropped_assets() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    // The release carries a kept asset plus assets the classifier drops
    // (android build, source-code archive). Only the kept subset is compared,
    // so the entry is still reused when the data is unchanged.
    let mut rel = release(
        "v1.19.9",
        false,
        vec![
            gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64))),
            gh_asset("mihomo-android-arm64-v1.19.9.gz", "https://example.com/android.gz", None),
            gh_asset("Source code (zip)", "https://example.com/source.zip", None),
        ],
    );
    rel["updated_at"] = json!("2026-07-30T00:00:00Z");
    rel["assets"][0]["updated_at"] = json!("2026-07-30T00:00:00Z");
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success();

    // Second run: identical data. The release must be REUSED (the summary
    // reports 1 reused) despite the dropped assets, and the kept asset's
    // sha256 stays untouched.
    server.reset().await;
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success().stderr(predicate::str::contains("1 reused"));

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let versions = index["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1);
    let assets = versions[0]["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 1, "only the classifier-kept asset is indexed");
    assert_eq!(assets[0]["name"], "mihomo-darwin-arm64-v1.19.9.gz");
    assert_eq!(assets[0]["sha256"], "a".repeat(64));
}

#[tokio::test]
async fn incremental_reprocesses_when_asset_removed_on_github() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    // First run: two kept assets with release and asset updated_at set.
    let mut rel = release(
        "v1.19.9",
        false,
        vec![
            gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64))),
            gh_asset("mihomo-linux-amd64-v1.19.9.gz", "https://example.com/l.gz", Some(&"b".repeat(64))),
        ],
    );
    rel["updated_at"] = json!("2026-07-30T00:00:00Z");
    rel["assets"][0]["updated_at"] = json!("2026-07-30T00:00:00Z");
    rel["assets"][1]["updated_at"] = json!("2026-07-30T00:00:00Z");
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success();

    // Second run: same release updated_at and the remaining asset is
    // unchanged, but the linux asset was deleted on GitHub. The kept-asset
    // sets no longer match, so the entry must be reprocessed and the removed
    // asset must disappear from the index.
    server.reset().await;
    let mut rel_new = rel.clone();
    rel_new["assets"] =
        json!([gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))]);
    rel_new["assets"][0]["updated_at"] = json!("2026-07-30T00:00:00Z");
    mount_releases_page(&server, json!([rel_new])).await;
    run_sync(&server.uri(), &out).success();

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let assets = index["versions"][0]["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 1, "removed asset must be dropped from the index");
    assert_eq!(assets[0]["name"], "mihomo-darwin-arm64-v1.19.9.gz");
    assert_eq!(assets[0]["sha256"], "a".repeat(64));
}

#[tokio::test]
async fn generated_at_kept_when_release_data_unchanged() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    let rel = release(
        "v1.19.9",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))],
    );
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success();
    let first = std::fs::read(&out).unwrap();

    // Identical API data: the second run must reproduce the same bytes
    // (previous generated_at reused), so the workflow's git-diff check
    // correctly skips the commit/upload.
    server.reset().await;
    mount_releases_page(&server, json!([rel])).await;
    run_sync(&server.uri(), &out).success();
    let second = std::fs::read(&out).unwrap();

    assert_eq!(second, first, "unchanged data must keep the previous generated_at (byte-identical output)");
}

#[tokio::test]
async fn generated_at_refreshes_when_release_data_changed() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    let rel = release(
        "v1.19.9",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))],
    );
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success();
    let first: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let first_ts = first["generated_at"].as_str().unwrap().to_string();

    // A new release appears: the timestamp must refresh to the run time.
    std::thread::sleep(std::time::Duration::from_millis(5));
    server.reset().await;
    let rel2 = release(
        "v1.19.10",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.10.gz", "https://example.com/b.gz", Some(&"b".repeat(64)))],
    );
    mount_releases_page(&server, json!([rel2, rel])).await;
    run_sync(&server.uri(), &out).success();
    let second: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let second_ts = second["generated_at"].as_str().unwrap().to_string();

    assert_ne!(second_ts, first_ts, "changed data must refresh generated_at");
    assert_eq!(second["versions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn classifier_config_change_triggers_reprocess() {
    init_logger();
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");

    // First run with the bundled classifier: darwin-arm64 -> darwin-aarch64.
    let rel = release(
        "v1.19.9",
        false,
        vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", Some(&"a".repeat(64)))],
    );
    mount_releases_page(&server, json!([rel.clone()])).await;
    run_sync(&server.uri(), &out).success();
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["versions"][0]["assets"][0]["platform"], "darwin-aarch64");

    // Second run with a classifier whose rules changed (same matching
    // pattern "darwin-arm64" now maps to a different canonical platform,
    // same release/asset timestamps): the entry must NOT be reused with the
    // stale platform — it must be reprocessed.
    let classifier = dir.path().join("renamed.json");
    std::fs::write(
        &classifier,
        r#"{
            "keep_extensions": ["gz"],
            "platforms": [{ "name": "linux-aarch64", "patterns": ["darwin-arm64"] }]
        }"#,
    )
    .unwrap();

    server.reset().await;
    mount_releases_page(&server, json!([rel])).await;
    run_sync_args(&server.uri(), &out, &["--classifier", classifier.to_str().unwrap()])
        .success()
        .stderr(predicate::str::contains("reprocessing"));

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index["versions"][0]["assets"][0]["platform"], "linux-aarch64");
}
