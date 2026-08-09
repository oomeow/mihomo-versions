use std::time::Duration;

use bytes::Bytes;
use reqwest::{StatusCode, header::HeaderMap};
use serde::de::DeserializeOwned;

use crate::error::Error;

const USER_AGENT: &str = concat!("mihomo-versions/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Shared HTTP client used by the library download path and the sync binary.
#[derive(Clone)]
pub struct HttpClient {
    /// Client for small requests (index fetch, JSON) with a total timeout.
    inner: reqwest::Client,
    /// Client for streaming downloads, without a total timeout: the download
    /// path enforces its own optional total timeout via
    /// [`crate::downloader::DownloadOptions::total_timeout`].
    download_inner: reqwest::Client,
}

impl HttpClient {
    pub fn new() -> Result<Self, Error> {
        Self::build_pair(None, HeaderMap::new())
    }

    /// Builds a client authenticated with a GitHub token.
    pub fn with_token(token: &str) -> Result<Self, Error> {
        Self::with_token_and_proxy(Some(token), None)
    }

    /// Builds a client that routes requests through an HTTP/HTTPS/SOCKS5 proxy.
    pub fn with_proxy(proxy: &str) -> Result<Self, Error> {
        Self::with_token_and_proxy(None, Some(proxy))
    }

    /// Builds a client with an optional token and an optional proxy.
    pub fn with_token_and_proxy(token: Option<&str>, proxy: Option<&str>) -> Result<Self, Error> {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            let value = format!("Bearer {token}");
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&value).map_err(|e| Error::InvalidToken(e.to_string()))?,
            );
        }
        let proxy = match proxy {
            Some(url) => Some(reqwest::Proxy::all(url).map_err(Error::Network)?),
            None => None,
        };
        Self::build_pair(proxy, headers)
    }

    fn build_pair(proxy: Option<reqwest::Proxy>, headers: HeaderMap) -> Result<Self, Error> {
        Ok(Self {
            inner: Self::builder(proxy.clone(), Some(TOTAL_TIMEOUT)).default_headers(headers.clone()).build()?,
            download_inner: Self::builder(proxy, None).default_headers(headers).build()?,
        })
    }

    fn builder(proxy: Option<reqwest::Proxy>, total_timeout: Option<Duration>) -> reqwest::ClientBuilder {
        let mut builder = reqwest::Client::builder().user_agent(USER_AGENT).connect_timeout(CONNECT_TIMEOUT);
        if let Some(timeout) = total_timeout {
            builder = builder.timeout(timeout);
        }
        match proxy {
            Some(proxy) => builder.proxy(proxy),
            None => builder,
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        log::debug!("GET {url}");
        let response = self.inner.get(url).send().await?;
        check_status(&response)?;
        log::debug!("GET {url} -> {}", response.status());
        Ok(response.json::<T>().await?)
    }

    pub async fn get_bytes(&self, url: &str) -> Result<Bytes, Error> {
        log::debug!("GET {url}");
        let response = self.inner.get(url).send().await?;
        check_status(&response)?;
        log::debug!("GET {url} -> {} ({} bytes)", response.status(), response.content_length().unwrap_or(0));
        Ok(response.bytes().await?)
    }

    /// Opens a streaming GET response for download. The caller consumes the
    /// body via `response.chunk().await`. No total timeout applies here; the
    /// download path enforces its own optional total timeout.
    pub async fn open(&self, url: &str) -> Result<reqwest::Response, Error> {
        log::debug!("GET {url} (stream)");
        let response = self.download_inner.get(url).send().await?;
        check_status(&response)?;
        log::debug!("GET {url} -> {}", response.status());
        Ok(response)
    }

    /// Opens a streaming GET with a `Range: bytes={start}-` header for resume.
    /// The response is returned without a status check so the caller can branch
    /// on 206 (partial), 200 (range ignored) and 416 (range unsatisfiable).
    /// No total timeout applies here; the download path enforces its own
    /// optional total timeout.
    pub async fn open_range(&self, url: &str, start: u64) -> Result<reqwest::Response, Error> {
        log::debug!("GET {url} (stream, Range: bytes={start}-)");
        let range = format!("bytes={start}-");
        let response = self.download_inner.get(url).header(reqwest::header::RANGE, range).send().await?;
        log::debug!("GET {url} -> {}", response.status());
        Ok(response)
    }

    /// Opens a GET with conditional headers (`If-None-Match` / `If-Modified-Since`)
    /// for cached index fetching. The response is returned without a status
    /// check so the caller can branch on 200 / 304 / other.
    pub(crate) async fn open_index(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<reqwest::Response, Error> {
        log::debug!("GET {url} (conditional)");
        let mut request = self.inner.get(url);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }
        let response = request.send().await?;
        log::debug!("GET {url} -> {}", response.status());
        Ok(response)
    }
}

fn check_status(response: &reqwest::Response) -> Result<(), Error> {
    if response.status().is_success() { Ok(()) } else { Err(status_to_error(response.status())) }
}

/// Maps an HTTP status to the corresponding error, treating 403/429 as rate
/// limited (GitHub surfaces rate limits with `x-ratelimit-remaining: 0`).
pub(crate) fn status_to_error(status: StatusCode) -> Error {
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN {
        Error::RateLimited
    } else {
        Error::Http(status.as_u16())
    }
}
