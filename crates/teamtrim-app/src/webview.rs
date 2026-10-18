//! The embedded browser used by the browser sign-in lane (ADR-042).
//!
//! `wry` gives us a native child webview (WKWebView on macOS, WebView2 on
//! Windows) attached to the eframe window. It loads the AAD authorize URL and
//! **intercepts the client's registered native-client redirect** in the
//! navigation handler: the callback URL is handed to the running sign-in over
//! the session channel, the navigation is cancelled, and the view is dropped
//! once the flow ends.
//!
//! This is the same technique better-teams uses: no system browser, no loopback
//! port, and the redirect URI is one the Teams first-party client actually has
//! registered (a loopback redirect is refused with `AADSTS50011`).
//!
//! ## Linux
//!
//! wry's Linux backend needs webkit2gtk, `gtk::init()` and a gtk main-loop pump
//! alongside eframe's event loop. That is deliberately **not built** here:
//! `wry` is a macOS/Windows dependency, and on Linux [`SignInWebView::open`]
//! fails so the login screen points at the device-code lane. See ADR-042.

/// The redirect the Teams client is registered for; the webview stops here.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub const NATIVE_CLIENT_REDIRECT: &str =
    "https://login.microsoftonline.com/common/oauth2/nativeclient";

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::mpsc::UnboundedSender;
    use wry::{Rect, WebView, WebViewBuilder};

    use super::NATIVE_CLIENT_REDIRECT;

    /// One embedded sign-in view, covering the app window until the flow ends.
    pub struct SignInWebView {
        webview: WebView,
        /// Set from the navigation handler once the callback was captured.
        done: Arc<AtomicBool>,
    }

    impl SignInWebView {
        /// Attach a webview to `frame` and load `url`.
        ///
        /// `callback` receives the intercepted redirect URL; `repaint` wakes
        /// egui so the next frame can drop the view and continue the flow.
        pub fn open(
            frame: &eframe::Frame,
            url: &str,
            size: (f32, f32),
            callback: UnboundedSender<String>,
            repaint: egui::Context,
        ) -> Result<Self, String> {
            let done = Arc::new(AtomicBool::new(false));
            let done_flag = Arc::clone(&done);
            let redirect = NATIVE_CLIENT_REDIRECT.to_string();

            let webview = WebViewBuilder::new()
                .with_url(url)
                .with_bounds(full_window(size))
                .with_navigation_handler(move |navigation: String| {
                    if navigation.starts_with(&redirect) {
                        // The code is in `navigation`; stop the navigation (the
                        // native-client page would only show the URL) and let
                        // the app finish the sign-in.
                        let _ = callback.send(navigation);
                        done_flag.store(true, Ordering::SeqCst);
                        repaint.request_repaint();
                        false
                    } else {
                        true
                    }
                })
                .build_as_child(frame)
                .map_err(|e| e.to_string())?;

            // The webview has focus so typing works immediately.
            let _ = webview.focus();
            Ok(Self { webview, done })
        }

        /// Keep the view covering the window (points; wry uses a top-left
        /// origin).
        pub fn resize(&self, size: (f32, f32)) {
            let _ = self.webview.set_bounds(full_window(size));
        }

        /// Whether the redirect was captured and the view can be dropped.
        pub fn is_done(&self) -> bool {
            self.done.load(Ordering::SeqCst)
        }
    }

    /// A rect covering the whole window, with a floor so a zero-sized frame
    /// during start-up cannot create a 0×0 view.
    fn full_window(size: (f32, f32)) -> Rect {
        let (width, height) = size;
        Rect {
            position: wry::dpi::LogicalPosition::new(0.0, 0.0).into(),
            size: wry::dpi::LogicalSize::new(width.max(1.0), height.max(1.0)).into(),
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// Linux build: no embedded browser (see the module docs).
    #[derive(Debug, Default)]
    pub struct SignInWebView;

    impl SignInWebView {
        /// Always fails; the login screen falls back to the device-code lane.
        pub fn open(
            _frame: &eframe::Frame,
            _url: &str,
            _size: (f32, f32),
            _callback: tokio::sync::mpsc::UnboundedSender<String>,
            _repaint: egui::Context,
        ) -> Result<Self, String> {
            Err("the embedded browser is not built on Linux; use device code".into())
        }

        /// No-op.
        pub fn resize(&self, _size: (f32, f32)) {}

        /// Nothing to wait for.
        pub fn is_done(&self) -> bool {
            true
        }
    }
}

pub use platform::SignInWebView;
