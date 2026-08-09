//! Shared test fixtures and builders for the integration tests.
//!
//! Every test builds its mock world from these builders so fixture shapes
//! change in one place instead of across dozens of hand-built JSON bodies.
//!
//! Each integration test file only uses a subset of these helpers, so the
//! unused ones must not warn (they are used by at least one test crate).
#![allow(dead_code)]

use std::{io::Write, time::Duration};

use assert_cmd::Command;
use flate2::{Compression, write::GzEncoder};
use mihomo_versions::{HttpClient, MihomoAsset};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

pub const OLD_RFC3339: &str = "2020-01-01T00:00:00Z";

pub fn gz_bytes(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

pub fn zip_bytes(data: &[u8]) -> Vec<u8> {
    use zip::write::SimpleFileOptions;
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    writer.start_file("mihomo", SimpleFileOptions::default()).unwrap();
    writer.write_all(data).unwrap();
    writer.finish().unwrap().into_inner()
}

pub fn tar_gz_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar_data = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_data);
        for &(name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        builder.finish().unwrap();
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar_data).unwrap();
    encoder.finish().unwrap()
}

pub fn zst_bytes(data: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(data, 1).unwrap()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// An indexed `MihomoAsset` as it appears in a version index.
pub fn asset(url: &str, sha256: Option<&str>, format: &str) -> MihomoAsset {
    MihomoAsset {
        name: format!("mihomo-darwin-arm64-v1.19.9.{format}"),
        platform: "darwin-arm64".to_string(),
        format: format.to_string(),
        size: None,
        sha256: sha256.map(str::to_string),
        created_at: None,
        updated_at: None,
        url: url.to_string(),
    }
}

/// A GitHub release asset as the API reports it.
pub fn gh_asset(name: &str, url: &str, digest: Option<&str>) -> serde_json::Value {
    let mut value = json!({
        "name": name,
        "size": 100,
        "browser_download_url": url,
    });
    if let Some(digest) = digest {
        value["digest"] = json!(format!("sha256:{digest}"));
    }
    value
}

/// A GitHub release as the API reports it.
pub fn release(tag: &str, prerelease: bool, assets: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "tag_name": tag,
        "prerelease": prerelease,
        "draft": false,
        "published_at": "2026-07-30T00:00:00Z",
        "assets": assets,
    })
}

/// Writes a stale `bin.part` plus its sidecar meta for resume tests.
pub fn part_setup(dir: &std::path::Path, data: &[u8], url: &str) {
    std::fs::write(dir.join("bin.part"), data).unwrap();
    std::fs::write(dir.join("bin.part.meta"), format!(r#"{{"url":"{url}"}}"#)).unwrap();
}

pub fn client() -> HttpClient {
    HttpClient::new().unwrap()
}

pub async fn mount_releases_page(server: &MockServer, releases: serde_json::Value) {
    mount_repo_releases(server, "MetaCubeX/mihomo", releases).await;
}

pub async fn mount_repo_releases(server: &MockServer, repo: &str, releases: serde_json::Value) {
    // page=1 serves the data; any later page returns an empty array so the
    // full-refresh sync terminates (registered first = takes priority).
    let releases_path = format!("/repos/{repo}/releases");
    Mock::given(method("GET"))
        .and(path(&releases_path))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(releases))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(&releases_path))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
}

pub fn run_sync(server_uri: &str, out: &std::path::Path) -> assert_cmd::assert::Assert {
    run_sync_args(server_uri, out, &[])
}

pub fn run_sync_args(server_uri: &str, out: &std::path::Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mihomo-versions-sync")
        .unwrap()
        .args(["--out", out.to_str().unwrap(), "--repo", "MetaCubeX/mihomo", "--api-base", server_uri])
        .args(extra)
        .assert()
}

pub async fn requests_for(server: &MockServer, path: &str) -> usize {
    server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == path).count()
}

pub fn index_json_body() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "generated_at": "2026-01-01T00:00:00Z",
        "versions": []
    })
}

pub fn now_rfc3339() -> String {
    mihomo_versions::now_rfc3339()
}

pub fn assert_part_files_clean(dir: &std::path::Path) {
    let leftovers = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".part"))
        .count();
    assert_eq!(leftovers, 0);
}

/// Initializes the logger once per test process so `log` output is visible when
/// running `cargo test -- --nocapture` (defaults to `debug`, overridable via
/// `RUST_LOG`).
pub fn init_logger() {
    use std::sync::OnceLock;
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).try_init();
    });
}

/// A minimal HTTP server that sends headers immediately but never the body, so
/// the client's next `chunk()` blocks until the idle timeout fires.
pub async fn spawn_stall_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let head = b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(head).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    format!("http://{addr}")
}
