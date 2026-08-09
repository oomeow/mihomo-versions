use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use tempfile::tempdir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

mod common;
use common::*;

#[tokio::test]
async fn batch_sync_processes_all_jobs() {
    let server = MockServer::start().await;

    mount_repo_releases(
        &server,
        "MetaCubeX/mihomo",
        json!([release(
            "v1.19.9",
            false,
            vec![gh_asset("mihomo-darwin-arm64-v1.19.9.gz", "https://example.com/a.gz", None)]
        )]),
    )
    .await;
    mount_repo_releases(
        &server,
        "madeye/meow-rs",
        json!([release(
            "v0.19.0",
            false,
            vec![gh_asset("meow-v0.19.0-aarch64-apple-darwin.tar.gz", "https://example.com/m.gz", None)]
        )]),
    )
    .await;

    let dir = tempdir().unwrap();
    let classifier = dir.path().join("meow-rs.json");
    std::fs::write(
        &classifier,
        r#"{
            "keep_extensions": ["gz", "zip"],
            "platforms": [{ "name": "darwin-aarch64", "patterns": ["aarch64-apple-darwin"] }]
        }"#,
    )
    .unwrap();
    let mihomo_out = dir.path().join("mihomo.json");
    let meow_out = dir.path().join("meow-rs.json").with_extension("out.json");
    let config = dir.path().join("sync-config.json");
    std::fs::write(
        &config,
        serde_json::to_string(&json!({
            "jobs": [
                { "repo": "MetaCubeX/mihomo", "out": mihomo_out.to_str() },
                { "repo": "madeye/meow-rs", "out": meow_out.to_str(), "classifier": classifier.to_str() }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .current_dir(dir.path())
        .args(["--config", "sync-config.json", "--api-base", &server.uri()])
        .assert()
        .success();

    let mihomo: serde_json::Value = serde_json::from_slice(&std::fs::read(&mihomo_out).unwrap()).unwrap();
    assert_eq!(mihomo["source"]["repo"], "mihomo");
    assert_eq!(mihomo["versions"][0]["channel"], "stable");

    let meow: serde_json::Value = serde_json::from_slice(&std::fs::read(&meow_out).unwrap()).unwrap();
    assert_eq!(meow["source"]["repo"], "meow-rs");
    assert_eq!(meow["versions"][0]["assets"][0]["format"], "tar.gz");
}

#[tokio::test]
async fn batch_sync_runs_remaining_jobs_and_reports_failures() {
    let server = MockServer::start().await;
    // First repo has no mock -> 404; second succeeds.
    mount_repo_releases(&server, "madeye/meow-rs", json!([release("v0.19.0", false, vec![])])).await;

    let dir = tempdir().unwrap();
    let a_out = dir.path().join("a.json");
    let b_out = dir.path().join("b.json");
    let config = dir.path().join("sync-config.json");
    std::fs::write(
        &config,
        serde_json::to_string(&json!({
            "jobs": [
                { "repo": "MetaCubeX/mihomo", "out": a_out.to_str() },
                { "repo": "madeye/meow-rs", "out": b_out.to_str() }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .current_dir(dir.path())
        .args(["--config", "sync-config.json", "--api-base", &server.uri()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("MetaCubeX/mihomo"))
        .stderr(predicate::str::contains("job(s) failed"));

    assert!(b_out.is_file(), "the healthy job must still complete");
    assert!(!a_out.exists());
}

#[tokio::test]
async fn batch_sync_classifier_load_failure_does_not_abort_batch() {
    let server = MockServer::start().await;
    // First job points at a classifier file that does not exist; the second
    // job has no classifier and must still run.
    mount_repo_releases(&server, "madeye/meow-rs", json!([release("v0.19.0", false, vec![])])).await;

    let dir = tempdir().unwrap();
    let a_out = dir.path().join("a.json");
    let b_out = dir.path().join("b.json");
    let config = dir.path().join("sync-config.json");
    std::fs::write(
        &config,
        serde_json::to_string(&json!({
            "jobs": [
                {
                    "repo": "MetaCubeX/mihomo",
                    "out": a_out.to_str(),
                    "classifier": dir.path().join("missing-classifier.json").to_str()
                },
                { "repo": "madeye/meow-rs", "out": b_out.to_str() }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .current_dir(dir.path())
        .args(["--config", "sync-config.json", "--api-base", &server.uri()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("MetaCubeX/mihomo"))
        .stderr(predicate::str::contains("classifier"))
        .stderr(predicate::str::contains("job(s) failed"));

    assert!(b_out.is_file(), "the healthy job must still complete after a classifier load failure");
    assert!(!a_out.exists());
}

#[tokio::test]
async fn batch_sync_rejects_empty_jobs() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("sync-config.json");
    std::fs::write(&config, r#"{"jobs": []}"#).unwrap();

    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .args(["--config", config.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("at least one job"));
}

#[tokio::test]
async fn batch_sync_reads_token_from_env() {
    let server = MockServer::start().await;
    // Only matches when the Authorization header carries the env-provided token;
    // otherwise the job gets a 404 and the run fails.
    let releases = json!([release("v1.19.9", false, vec![])]);
    Mock::given(method("GET"))
        .and(path("/repos/MetaCubeX/mihomo/releases"))
        .and(query_param("page", "1"))
        .and(header("authorization", "Bearer env-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(releases))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/MetaCubeX/mihomo/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    let config = dir.path().join("sync-config.json");
    std::fs::write(
        &config,
        serde_json::to_string(&json!({ "jobs": [{ "repo": "MetaCubeX/mihomo", "out": out.to_str() }] })).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .env("MIHOMO_VERSION_TOKEN", "env-token")
        .args(["--config", config.to_str().unwrap(), "--api-base", &server.uri()])
        .assert()
        .success();
    assert!(out.is_file());
}

#[tokio::test]
async fn batch_sync_compact_applies_to_all_jobs() {
    let server = MockServer::start().await;
    mount_repo_releases(&server, "MetaCubeX/mihomo", json!([release("v1.19.9", false, vec![])])).await;

    let dir = tempdir().unwrap();
    let out = dir.path().join("index.json");
    let config = dir.path().join("sync-config.json");
    std::fs::write(
        &config,
        serde_json::to_string(&json!({ "jobs": [{ "repo": "MetaCubeX/mihomo", "out": out.to_str() }] })).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--api-base", &server.uri(), "--compact"])
        .assert()
        .success();

    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!text.contains('\n'), "batch compact output must be a single line");
    let _: serde_json::Value = serde_json::from_str(&text).unwrap();
}
