use std::time::Duration;

use mihomo_versions::Error;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

mod common;
use common::*;

#[tokio::test]
async fn downloads_decompresses_and_verifies() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"mihomo fake binary contents";
    let compressed = gz_bytes(data);
    let digest = sha256_hex(&compressed);

    Mock::given(method("GET"))
        .and(path("/bin.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed.clone()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();

    let dest = dir.path().join("mihomo");
    let progress = std::cell::Cell::new((0u64, 0u64));
    let url = format!("{}/bin.gz", server.uri());
    mihomo_versions::download(
        &client,
        &asset(&url, Some(&digest), "gz"),
        &dest,
        mihomo_versions::DownloadOptions::default(),
        |downloaded, total| progress.set((downloaded, total)),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(progress.get(), (compressed.len() as u64, compressed.len() as u64));
    assert_part_files_clean(dir.path());

    // Missing sha256: warn-and-continue path.
    let dest2 = dir.path().join("mihomo2");
    mihomo_versions::download(
        &client,
        &asset(&url, None, "gz"),
        &dest2,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest2).unwrap(), data);

    // Mismatched sha256: hard error.
    let dest3 = dir.path().join("mihomo3");
    let wrong = "0".repeat(64);
    let result = mihomo_versions::download(
        &client,
        &asset(&url, Some(&wrong), "gz"),
        &dest3,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await;
    assert!(matches!(result, Err(Error::ChecksumMismatch { .. })));
}

#[tokio::test]
async fn downloads_and_extracts_zip() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"windows binary";
    let compressed = zip_bytes(data);

    Mock::given(method("GET"))
        .and(path("/bin.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("mihomo.exe");
    let url = format!("{}/bin.zip", server.uri());
    mihomo_versions::download(
        &client,
        &asset(&url, None, "zip"),
        &dest,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn downloads_raw_executable_without_decompression() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"\x7fELF fake raw executable";
    Mock::given(method("GET"))
        .and(path("/bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.to_vec()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("mihomo");
    let url = format!("{}/bin", server.uri());
    let mut raw = asset(&url, Some(&sha256_hex(data)), "raw");
    raw.name = "mihomo".into();
    mihomo_versions::download(&client, &raw, &dest, mihomo_versions::DownloadOptions::default(), |_, _| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_part_files_clean(dir.path());
}

#[tokio::test]
async fn downloads_and_extracts_tar_gz() {
    init_logger();
    let server = MockServer::start().await;
    let binary: &[u8] = b"\x7fELF meow binary";
    let archive = tar_gz_bytes(&[("meow", binary)]);

    Mock::given(method("GET"))
        .and(path("/meow.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive.clone()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("meow");
    let url = format!("{}/meow.tar.gz", server.uri());
    let mut meow = asset(&url, Some(&sha256_hex(&archive)), "tar.gz");
    meow.name = "meow-v0.19.0-aarch64-apple-darwin.tar.gz".into();
    mihomo_versions::download(&client, &meow, &dest, mihomo_versions::DownloadOptions::default(), |_, _| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), binary);
    assert_part_files_clean(dir.path());
}

#[tokio::test]
async fn downloads_and_decompresses_zst() {
    init_logger();
    let server = MockServer::start().await;
    let binary: &[u8] = b"\x7fELF zstd binary contents";
    let compressed = zst_bytes(binary);

    Mock::given(method("GET"))
        .and(path("/bin.zst"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bin-corrupt.zst"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not zstd data".to_vec()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("mihomo");
    let url = format!("{}/bin.zst", server.uri());
    mihomo_versions::download(
        &client,
        &asset(&url, Some(&sha256_hex(&compressed)), "zst"),
        &dest,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), binary);
    assert_part_files_clean(dir.path());

    // Corrupt zstd bytes must surface a clear error.
    let dest2 = dir.path().join("mihomo2");
    let corrupt: &[u8] = b"not zstd data";
    let corrupt_url = format!("{}/bin-corrupt.zst", server.uri());
    let result = mihomo_versions::download(
        &client,
        &asset(&corrupt_url, Some(&sha256_hex(corrupt)), "zst"),
        &dest2,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await;
    assert!(matches!(result, Err(Error::ArchiveContent(_))));
}

#[tokio::test]
async fn downloads_resume_from_partial_part() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"0123456789";
    Mock::given(method("GET"))
        .and(path("/bin"))
        .and(header("range", "bytes=5-"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(data[5..].to_vec()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");
    let url = format!("{}/bin", server.uri());
    part_setup(dir.path(), &data[..5], &url);

    mihomo_versions::download(
        &client,
        &asset(&url, Some(&sha256_hex(data)), "raw"),
        &dest,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_part_files_clean(dir.path());
}

#[tokio::test]
async fn downloads_restart_when_server_ignores_range() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"0123456789";
    Mock::given(method("GET"))
        .and(path("/bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.to_vec()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");
    let url = format!("{}/bin", server.uri());
    part_setup(dir.path(), b"01234", &url);

    mihomo_versions::download(
        &client,
        &asset(&url, Some(&sha256_hex(data)), "raw"),
        &dest,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn download_skips_when_part_already_complete() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"0123456789";
    Mock::given(method("GET")).and(path("/bin")).respond_with(ResponseTemplate::new(416)).mount(&server).await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");
    let url = format!("{}/bin", server.uri());
    part_setup(dir.path(), data, &url);

    mihomo_versions::download(
        &client,
        &asset(&url, Some(&sha256_hex(data)), "raw"),
        &dest,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn download_discards_stale_partial_for_different_asset() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"0123456789";
    Mock::given(method("GET"))
        .and(path("/bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.to_vec()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");
    let url = format!("{}/bin", server.uri());
    // A partial left by a previous, different asset (different url) must not be resumed.
    part_setup(dir.path(), b"stale", &format!("{url}other"));

    mihomo_versions::download(
        &client,
        &asset(&url, Some(&sha256_hex(data)), "raw"),
        &dest,
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data, "stale partial must not contaminate the new asset");
    assert_part_files_clean(dir.path());
}

#[tokio::test]
async fn download_can_be_cancelled() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"some-body".to_vec()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");
    let token = CancellationToken::new();
    token.cancel();

    let url = format!("{}/bin", server.uri());
    let opts =
        mihomo_versions::DownloadOptions { cancel: Some(token), idle_timeout: None, total_timeout: None, resume: true };
    let result = mihomo_versions::download(&client, &asset(&url, None, "raw"), &dest, opts, |_, _| {}).await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!dest.exists(), "cancelled download must not produce the final file");
}

#[tokio::test]
async fn download_cancels_an_in_flight_read() {
    init_logger();
    // Headers arrive but the body never does, so the download is blocked inside
    // `response.chunk()`; cancelling must abort that in-flight read.
    let url = spawn_stall_server().await;
    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");
    let token = CancellationToken::new();

    let opts = mihomo_versions::DownloadOptions {
        cancel: Some(token.clone()),
        idle_timeout: None,
        total_timeout: None,
        resume: false,
    };
    let dest_task = dest.clone();
    let handle = tokio::spawn(async move {
        mihomo_versions::download(&client, &asset(&format!("{url}/bin"), None, "raw"), &dest_task, opts, |_, _| {})
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    token.cancel();

    let result = handle.await.unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!dest.exists());
}

#[tokio::test]
async fn download_times_out_on_idle() {
    init_logger();
    let url = spawn_stall_server().await;
    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");

    let opts = mihomo_versions::DownloadOptions {
        idle_timeout: Some(Duration::from_millis(100)),
        cancel: None,
        total_timeout: None,
        resume: false,
    };
    let result =
        mihomo_versions::download(&client, &asset(&format!("{url}/bin"), None, "raw"), &dest, opts, |_, _| {}).await;
    assert!(matches!(result, Err(Error::Timeout)));
}

#[tokio::test]
async fn download_times_out_on_total_timeout() {
    init_logger();
    // Headers arrive but the body never does; with no idle timeout set, only
    // the total timeout can fire.
    let url = spawn_stall_server().await;
    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");

    let opts = mihomo_versions::DownloadOptions {
        total_timeout: Some(Duration::from_millis(100)),
        idle_timeout: None,
        cancel: None,
        resume: false,
    };
    let result =
        mihomo_versions::download(&client, &asset(&format!("{url}/bin"), None, "raw"), &dest, opts, |_, _| {}).await;
    assert!(matches!(result, Err(Error::Timeout)));
}

#[tokio::test]
async fn download_async_completes_in_background() {
    init_logger();
    let server = MockServer::start().await;
    let data: &[u8] = b"hello async";
    Mock::given(method("GET"))
        .and(path("/bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.to_vec()))
        .mount(&server)
        .await;

    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");
    let url = format!("{}/bin", server.uri());
    let handle = mihomo_versions::download_async(
        &client,
        asset(&url, Some(&sha256_hex(data)), "raw"),
        dest.clone(),
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    );
    assert!(!handle.is_cancelled());
    handle.wait().await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn download_async_can_cancel_and_wait() {
    init_logger();
    let url = spawn_stall_server().await;
    let client = client();
    let dir = tempdir().unwrap();
    let dest = dir.path().join("bin");

    let handle = mihomo_versions::download_async(
        &client,
        asset(&format!("{url}/bin"), None, "raw"),
        dest.clone(),
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel();
    assert!(handle.is_cancelled());
    let result = handle.wait().await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!dest.exists());
}

#[test]
fn proxy_client_constructs() {
    init_logger();
    mihomo_versions::HttpClient::with_proxy("http://127.0.0.1:1").unwrap();
    mihomo_versions::HttpClient::with_token_and_proxy(Some("tok"), Some("socks5://127.0.0.1:1")).unwrap();
    assert!(mihomo_versions::HttpClient::with_proxy("://not-a-proxy").is_err());
}

#[tokio::test]
async fn download_404_reports_asset_with_context() {
    init_logger();
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/gone.gz")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

    let client = client();
    let dir = tempdir().unwrap();
    let url = format!("{}/gone.gz", server.uri());
    let result = mihomo_versions::download(
        &client,
        &asset(&url, None, "gz"),
        &dir.path().join("mihomo"),
        mihomo_versions::DownloadOptions::default(),
        |_, _| {},
    )
    .await;
    match result {
        Err(Error::AssetUnavailable { name, url: err_url }) => {
            assert_eq!(name, "mihomo-darwin-arm64-v1.19.9.gz");
            assert_eq!(err_url, url);
        }
        other => panic!("expected AssetUnavailable, got {other:?}"),
    }
    assert_part_files_clean(dir.path());
}
