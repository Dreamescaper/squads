//! The loopback HTTP listener that completes the browser (PKCE) sign-in flow.
//!
//! RFC 8252 §7.3: a native app receives the authorization code on a **loopback
//! redirect** — a short-lived HTTP server on `127.0.0.1`/`::1` that the browser
//! hits after the user signs in. This module is the transport-free half of that:
//! it owns the socket, serves the one redirect and hands the parsed
//! [`RedirectOutcome`] back to [`sign_in_browser`](super::flow::AuthDriver::sign_in_browser),
//! which validates `state` and redeems the code.
//!
//! The listener is deliberately dumb: it accepts connections until a request
//! carries a `code`/`error` query (anything else, e.g. `/favicon.ico`, is
//! answered `404` and ignored), then it stops. `state` validation belongs to the
//! driver, so a stray local request cannot complete a sign-in by itself.
//!
//! `redirect_uri` uses the host `localhost` with the ephemeral port — the form
//! Azure AD documents for desktop apps — while the socket is bound to both loop
//! addresses, because a browser may resolve `localhost` to `::1` or `127.0.0.1`
//! (RFC 8252 §7.3 warns about exactly this).

use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::pkce::{RedirectOutcome, parse_redirect};
use crate::error::{CoreError, Result};

/// How long the browser has to reach the loopback redirect before the flow
/// gives up and the sign-in fails.
pub const DEFAULT_BROWSER_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest request head accepted, so a stray client cannot make us buffer
/// forever.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long one accepted connection may take to send its request head.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A loopback listener bound to an ephemeral port on both `127.0.0.1` and (when
/// available) `::1`.
///
/// Bind it, send [`LoopbackListener::redirect_uri`] to AAD as the
/// `redirect_uri`, then await [`LoopbackListener::wait_for_redirect`].
pub struct LoopbackListener {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl LoopbackListener {
    /// Bind an ephemeral port on the loopback interface(s).
    pub async fn bind() -> Result<Self> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| CoreError::Auth(format!("could not open the loopback port: {e}")))?;
        let port = v4
            .local_addr()
            .map_err(|e| CoreError::Auth(format!("loopback address unavailable: {e}")))?
            .port();
        // IPv6 is best effort: a host without it still gets a working flow.
        let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await.ok();
        tracing::debug!(port, ipv6 = v6.is_some(), "loopback listener bound");
        Ok(Self { v4, v6, port })
    }

    /// The port the browser must reach.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `redirect_uri` to send to AAD (`http://localhost:<port>`).
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Serve the browser's redirect and return the parsed outcome, or fail when
    /// `timeout` elapses first.
    ///
    /// Requests that carry neither a `code` nor an `error` (a favicon probe, a
    /// port scan) are answered `404` and the listener keeps waiting.
    pub async fn wait_for_redirect(&self, timeout: Duration) -> Result<RedirectOutcome> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let stream = match tokio::time::timeout_at(deadline, self.accept()).await {
                Err(_) => {
                    return Err(CoreError::Auth(
                        "timed out waiting for the browser sign-in to complete".into(),
                    ));
                }
                Ok(Err(e)) => {
                    return Err(CoreError::Auth(format!("loopback listener failed: {e}")));
                }
                Ok(Ok(stream)) => stream,
            };
            match answer(stream).await {
                Ok(Some(outcome)) => return Ok(outcome),
                // Not the redirect: keep serving until the deadline.
                Ok(None) | Err(_) => continue,
            }
        }
    }

    /// Accept one connection from either loop address.
    async fn accept(&self) -> std::io::Result<TcpStream> {
        match &self.v6 {
            Some(v6) => tokio::select! {
                accepted = self.v4.accept() => accepted.map(|(stream, _)| stream),
                accepted = v6.accept() => accepted.map(|(stream, _)| stream),
            },
            None => self.v4.accept().await.map(|(stream, _)| stream),
        }
    }
}

impl std::fmt::Debug for LoopbackListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackListener")
            .field("port", &self.port)
            .field("ipv6", &self.v6.is_some())
            .finish()
    }
}

/// Read one request, answer it, and report whether it was the OAuth redirect.
async fn answer(mut stream: TcpStream) -> Result<Option<RedirectOutcome>> {
    let target = request_target(&mut stream).await?;
    match parse_redirect(&target) {
        Ok(outcome) => {
            write_response(&mut stream, "200 OK", &callback_page(&outcome)).await;
            Ok(Some(outcome))
        }
        Err(_) => {
            write_response(&mut stream, "404 Not Found", "<h1>404</h1>").await;
            Ok(None)
        }
    }
}

/// The request target (path + query) of an HTTP request head.
async fn request_target(stream: &mut TcpStream) -> Result<String> {
    let head = read_head(stream).await?;
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_string)
        .ok_or_else(|| CoreError::Auth("malformed HTTP request on the loopback port".into()))
}

/// Read until the end of the request head (or the cap).
async fn read_head(stream: &mut TcpStream) -> Result<String> {
    let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let read = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(CoreError::Auth(format!("loopback read failed: {e}"))),
            Err(_) => return Err(CoreError::Auth("loopback request timed out".into())),
        };
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Send a response and close the connection.
async fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The page the browser shows after the redirect. Never echoes the code.
fn callback_page(outcome: &RedirectOutcome) -> String {
    let (title, detail) = match outcome {
        RedirectOutcome::Code { .. } => (
            "Sign-in complete",
            "You can close this tab and return to TeamTrim.".to_string(),
        ),
        RedirectOutcome::Error { error, .. } => (
            "Sign-in failed",
            // `error` is an AAD error code (e.g. `access_denied`); strip
            // anything else so the page stays inert.
            format!(
                "TeamTrim could not complete the sign-in ({}). You can close this tab and try again.",
                sanitize(error)
            ),
        ),
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;margin:4rem auto;max-width:32rem\">\
         <h1>{title}</h1><p>{detail}</p></body></html>"
    )
}

/// Keep only characters that can appear in an OAuth error code.
fn sanitize(error: &str) -> String {
    let safe: String = error
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    if safe.is_empty() {
        "unknown_error".to_string()
    } else {
        safe
    }
}

/// Test support shared by the flow and session tests: act like the browser that
/// hits the loopback redirect.
///
/// Reads `redirect_uri`/`state` from the authorize URL, then GETs the callback
/// with `query`, substituting `{state}`. Runs on its own task so the flow can
/// reach `wait_for_redirect` first.
#[cfg(test)]
pub(crate) fn respond_like_a_browser(authorize_url: &str, query: &str) {
    let parsed = url::Url::parse(authorize_url).expect("authorize url");
    let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    let redirect = params.get("redirect_uri").expect("redirect_uri").clone();
    let state = params.get("state").expect("state").clone();
    let callback = format!(
        "{}/?{}",
        redirect.trim_end_matches('/'),
        query.replace("{state}", &state)
    );
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let url = url::Url::parse(&callback).expect("callback url");
        let host = url.host_str().expect("host").to_string();
        let port = url.port().expect("port");
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        let mut stream = match TcpStream::connect((host.as_str(), port)).await {
            Ok(stream) => stream,
            Err(e) => panic!("browser could not reach the loopback listener: {e}"),
        };
        let request = format!("GET {target} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
        use tokio::io::AsyncWriteExt as _;
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write callback");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .expect("connect");
        let request = format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        response
    }

    #[tokio::test]
    async fn redirect_uri_is_a_loopback_url_on_the_bound_port() {
        let listener = LoopbackListener::bind().await.expect("bind");
        assert!(listener.port() > 0);
        assert_eq!(
            listener.redirect_uri(),
            format!("http://localhost:{}", listener.port())
        );
    }

    #[tokio::test]
    async fn serves_the_callback_and_returns_the_code() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let port = listener.port();
        let browser = tokio::spawn(async move { get(port, "/?code=CODE-1&state=ST-1").await });

        let outcome = listener
            .wait_for_redirect(Duration::from_secs(5))
            .await
            .expect("outcome");
        assert_eq!(
            outcome,
            RedirectOutcome::Code {
                code: "CODE-1".into(),
                state: Some("ST-1".into())
            }
        );
        let page = browser.await.expect("browser task");
        assert!(page.starts_with("HTTP/1.1 200 OK"), "page = {page}");
        assert!(page.contains("Sign-in complete"));
        assert!(!page.contains("CODE-1"), "the code must not be echoed");
    }

    #[tokio::test]
    async fn ignores_stray_requests_until_the_redirect_arrives() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let port = listener.port();
        let browser = tokio::spawn(async move {
            // A favicon probe first: it must not complete the flow.
            let favicon = get(port, "/favicon.ico").await;
            assert!(favicon.starts_with("HTTP/1.1 404"), "got {favicon}");
            get(port, "/?error=access_denied").await
        });

        let outcome = listener
            .wait_for_redirect(Duration::from_secs(5))
            .await
            .expect("outcome");
        assert!(matches!(outcome, RedirectOutcome::Error { .. }));
        let page = browser.await.expect("browser task");
        assert!(page.contains("Sign-in failed"));
        assert!(page.contains("access_denied"));
    }

    #[tokio::test]
    async fn times_out_when_the_browser_never_returns() {
        let listener = LoopbackListener::bind().await.expect("bind");
        let err = listener
            .wait_for_redirect(Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("timed out"), "got {err}");
    }
}
