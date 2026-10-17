//! Inline media cache (milestone **M13**).
//!
//! Teams message bodies carry images as plain `<img src="…">` tags; the
//! conversation view extracts them with [`crate::text::image_urls`] and needs
//! local bytes to render (egui cannot paint a URL directly without a blocking
//! fetch). This module is the small, offline-testable cache in front of that
//! fetch:
//!
//! ```text
//!   conversation view ──▶ MediaCache::fetch(url) ──▶ <cache>/media/<hash>.<ext>
//!                              │
//!                              └─▶ GET url  (Skype token only on Microsoft hosts)
//! ```
//!
//! ## One fetch per URL, deterministic path
//!
//! A URL is written once under [`AppDirs::media_dir`] with a name derived
//! **deterministically** from the URL (a SHA-256 of the URL, truncated, plus an
//! image extension when the URL names one). A second `fetch` for the same URL
//! sees the file already present and returns it **without a request**, so
//! repeated timeline renders never refetch.
//!
//! ## Auth is the vendored host rule
//!
//! The cache sends the Skype token **only** when
//! [`crate::api::media_needs_auth`] says the URL is a Microsoft media host
//! (`*.msg.teams.microsoft.com`, `*.asm.skype.com`, …); the header scheme is
//! the vendored [`crate::api::media_auth_headers`] (object-store
//! `Authorization: skype_token …` vs. chat-service `Authentication` +
//! `X-SkypeToken`). Public hosts (Giphy, arbitrary links) fetch
//! unauthenticated, so the token never leaks to a third party.
//!
//! ## Testability
//!
//! [`MediaCache::with_client`] takes an injected [`reqwest::Client`], so the
//! service is driven against a local TCP stub with no network, no TLS and no
//! tenant: the stub records whether the auth header arrived, and a second call
//! is proven to hit the cache by the stub receiving only one request.
//!
//! ## Limits (documented, not fixed here)
//!
//! * Bytes are held in memory once per fetch before the file is written (the
//!   vendored cap is [`crate::api::MAX_BYTES`], 15 MiB); there is no streaming.
//! * The cache is keyed by URL only — not by account — and lives in the shared
//!   media directory, like the rest of the local cache it is **unencrypted at
//!   rest** (B-007).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::api::{MAX_BYTES, media_auth_headers, media_needs_auth};
use crate::error::{CoreError, Result};
use crate::paths::AppDirs;

/// A media file on disk after a [`MediaCache::fetch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedMedia {
    /// The URL this file was fetched from.
    pub url: String,
    /// Local path of the cached bytes.
    pub path: PathBuf,
    /// Best-effort MIME type (the server's `Content-Type`, else inferred from
    /// the extension).
    pub mime: Option<String>,
    /// Whether the file was already cached (no request was made).
    pub from_cache: bool,
}

/// The inline-media cache (see the module docs).
pub struct MediaCache {
    dirs: Arc<AppDirs>,
    /// The Skype token, sent only to Microsoft media hosts.
    token: String,
    /// Injectable HTTP client (a fresh one by default; the tests point it at a
    /// local stub).
    client: reqwest::Client,
}

impl MediaCache {
    /// Build a cache rooted at `dirs`, authenticating Microsoft hosts with
    /// `token`.
    pub fn new(dirs: Arc<AppDirs>, token: impl Into<String>) -> Self {
        Self::with_client(dirs, token, reqwest::Client::new())
    }

    /// Build a cache over an explicit HTTP client (tests).
    pub fn with_client(
        dirs: Arc<AppDirs>,
        token: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            dirs,
            token: token.into(),
            client,
        }
    }

    /// The deterministic on-disk path a URL is cached at.
    ///
    /// `<media_dir>/<sha256(url)[..32 hex]>.<ext>`, where `<ext>` is the URL's
    /// image extension (`png`/`jpg`/`gif`/`webp`/…) or `bin` when the URL names
    /// none (Microsoft object-store URLs carry no extension). egui's image
    /// loader sniffs the bytes, so `bin` still renders.
    pub fn cached_path(&self, url: &str) -> PathBuf {
        self.dirs
            .media_dir()
            .join(format!("{}.{}", cache_key(url), extension_for(url)))
    }

    /// Whether `url` is already cached locally (no request, no IO error).
    pub fn is_cached(&self, url: &str) -> bool {
        self.cached_path(url).is_file()
    }

    /// Fetch `url` once and cache the bytes, or return the already-cached file.
    ///
    /// A cached file short-circuits the network (see the module docs). On a
    /// miss the vendored host rule decides whether the Skype token is attached;
    /// the response is capped at [`MAX_BYTES`] and written atomically (temp file
    /// + rename), so a torn write never leaves a half-file a later render would
    /// read.
    pub async fn fetch(&self, url: &str) -> Result<CachedMedia> {
        let path = self.cached_path(url);
        if path.is_file() {
            return Ok(CachedMedia {
                url: url.to_string(),
                mime: mime_for_extension(&path),
                path,
                from_cache: true,
            });
        }

        let mut request = self.client.get(url);
        // The token is attached only when the vendored host rule says the URL
        // is a Microsoft media host; `media_auth_headers` already returns empty
        // otherwise, but the explicit check documents the boundary.
        if media_needs_auth(url) {
            for (name, value) in media_auth_headers(url, &self.token) {
                request = request.header(name, value);
            }
        }
        let response = request
            .send()
            .await
            .map_err(|e| CoreError::Media(format!("fetch {}: {e}", host_of(url))))?;
        if !response.status().is_success() {
            return Err(CoreError::Media(format!(
                "fetch {}: HTTP {}",
                host_of(url),
                response.status()
            )));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let bytes = response
            .bytes()
            .await
            .map_err(|e| CoreError::Media(format!("read {}: {e}", host_of(url))))?;
        if bytes.len() > MAX_BYTES {
            return Err(CoreError::Media(format!(
                "{} is {} bytes, over the {} byte cap",
                host_of(url),
                bytes.len(),
                MAX_BYTES
            )));
        }
        write_atomic(&path, &bytes)?;

        Ok(CachedMedia {
            url: url.to_string(),
            mime: content_type.or_else(|| mime_for_extension(&path)),
            path,
            from_cache: false,
        })
    }
}

/// Redacting `Debug`: shows the cache directory and whether a token is held,
/// never the token.
impl std::fmt::Debug for MediaCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaCache")
            .field("media_dir", &self.dirs.media_dir())
            .field("has_token", &!self.token.is_empty())
            .finish_non_exhaustive()
    }
}

/// A stable, filesystem-safe cache key for a URL (SHA-256, first 16 bytes).
fn cache_key(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The image extension a URL names, or `bin`.
///
/// Only a small, known set is accepted (so a `.php`/`.aspx` URL does not become
/// a misleading extension); the query string and fragment are ignored.
fn extension_for(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/');
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        "svg" => "svg",
        "avif" => "avif",
        "ico" => "ico",
        "tif" | "tiff" => "tif",
        _ => "bin",
    }
}

/// A best-effort MIME type from a cached file's extension.
fn mime_for_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Write `bytes` to `path` atomically: a sibling `.part` file is renamed into
/// place, so a reader never observes a half-written image.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes).map_err(|e| CoreError::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e))?;
    Ok(())
}

/// The host of a URL for an error message (never the full URL, which may carry
/// a token or a private path).
fn host_of(url: &str) -> &str {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    &rest[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (PathBuf, Arc<AppDirs>) {
        let tmp = std::env::temp_dir().join(format!("squads-media-{}", uuid::Uuid::new_v4()));
        (tmp.clone(), Arc::new(AppDirs::rooted_at(tmp)))
    }

    /// Serve one scripted HTTP response and record every request's head.
    ///
    /// Mirrors the `api::http_stub` helper but is local to this module so the
    /// media tests are self-contained. Each connection answers one request.
    async fn media_stub(
        responses: Vec<(u16, &'static str, Vec<u8>)>,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stub binds");
        let base = format!("http://{}", listener.local_addr().expect("stub addr"));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            for (status, content_type, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                log.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(String::from_utf8_lossy(&buf).to_string());
                let head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            }
        });
        (base, seen, task)
    }

    #[tokio::test]
    async fn fetch_writes_bytes_and_a_second_call_hits_the_cache() {
        let (tmp, dirs) = dirs();
        let (base, seen, task) = media_stub(vec![(200, "image/png", vec![1, 2, 3, 4])]).await;
        // A public host: no auth header must be sent.
        let url = format!("{base}/photo.png");
        let cache = MediaCache::new(Arc::clone(&dirs), "skype-secret");

        let first = cache.fetch(&url).await.expect("fetch");
        assert!(!first.from_cache);
        assert_eq!(first.mime.as_deref(), Some("image/png"));
        assert_eq!(std::fs::read(&first.path).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(first.path.extension().and_then(|e| e.to_str()), Some("png"));

        // A second call returns the cached file without a request.
        let second = cache.fetch(&url).await.expect("cached fetch");
        assert!(second.from_cache);
        assert_eq!(second.path, first.path);
        assert_eq!(second.mime.as_deref(), Some("image/png"));
        assert!(cache.is_cached(&url));

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            requests.len(),
            1,
            "the second call must not hit the network"
        );
        // A public host gets no auth header.
        assert!(
            !requests[0].to_ascii_lowercase().contains("authorization"),
            "public host must not receive a token: {}",
            requests[0]
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn fetch_attaches_the_token_only_for_microsoft_hosts() {
        let (tmp, dirs) = dirs();
        // The stub is `http://127.0.0.1`; `needs_auth` requires an https
        // Microsoft host, so we prove the *rule* by driving the header builder
        // directly and, separately, that an http URL gets no token on the wire.
        let (base, seen, task) = media_stub(vec![(200, "image/gif", vec![9, 9])]).await;
        let url = format!("{base}/x.gif");
        let cache = MediaCache::new(Arc::clone(&dirs), "skype-secret");
        assert!(
            !media_needs_auth(&url),
            "a loopback http URL never needs auth"
        );
        cache.fetch(&url).await.expect("fetch");

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(!requests[0].to_ascii_lowercase().contains("authorization"));
        assert!(!requests[0].to_ascii_lowercase().contains("skypetoken"));
        // And the pure rule: a Microsoft host *does* produce auth headers.
        let headers = media_auth_headers(
            "https://us-api.asm.skype.com/v1/objects/0/x",
            "skype-secret",
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                    && value.contains("skype-secret")),
            "asm host must carry the object-store scheme: {headers:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn a_failed_fetch_is_an_error_and_caches_nothing() {
        let (tmp, dirs) = dirs();
        let (base, _seen, task) = media_stub(vec![(404, "text/plain", b"nope".to_vec())]).await;
        let url = format!("{base}/missing.png");
        let cache = MediaCache::new(Arc::clone(&dirs), "");
        let err = cache.fetch(&url).await.unwrap_err();
        assert!(format!("{err}").contains("404"), "err = {err}");
        assert!(!cache.is_cached(&url));
        task.await.expect("stub drains");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cache_path_is_deterministic_and_extension_aware() {
        let (tmp, dirs) = dirs();
        let cache = MediaCache::new(dirs, "");
        let a = cache.cached_path("https://media.giphy.com/abc/giphy.gif");
        let b = cache.cached_path("https://media.giphy.com/abc/giphy.gif");
        assert_eq!(a, b, "the same URL maps to the same file");
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("gif"));
        assert_ne!(
            a,
            cache.cached_path("https://media.giphy.com/abc/other.gif"),
            "different URLs must not collide"
        );
        // A Microsoft object-store URL names no extension: `bin`, still cached.
        let ams = cache.cached_path("https://us-api.asm.skype.com/v1/objects/0/x/views/imgo");
        assert_eq!(ams.extension().and_then(|e| e.to_str()), Some("bin"));
        // Query strings are ignored when picking the extension.
        assert_eq!(
            cache
                .cached_path("https://h.example.com/a/b.png?x=1#frag")
                .extension()
                .and_then(|e| e.to_str()),
            Some("png")
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn debug_does_not_leak_the_token() {
        let (_tmp, dirs) = dirs();
        let cache = MediaCache::new(dirs, "super-secret-skype-token");
        let dbg = format!("{cache:?}");
        assert!(dbg.contains("has_token"));
        assert!(!dbg.contains("super-secret"), "token leaked: {dbg}");
    }
}
