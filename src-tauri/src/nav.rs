// Where the launcher's webview may go: the bundled page, blizznux.com in the forum frame, the
// widget frame its sign-up page embeds, and everything else in the user's browser. No other page
// is ever rendered inside the launcher.
// GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::process::Stdio;

use tauri::plugin::{Builder, TauriPlugin};
use url::Url;

/// What to do with one navigation request.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Nav {
    /// Load it in the launcher.
    Allow,
    /// Cancel it and hand the address to the user's browser.
    External,
    /// Cancel it and do nothing more.
    Refuse,
}

/// The hosts a frame may show besides our own page. blizznux.com is the forum; the Turnstile host
/// is the widget the site embeds in a frame on its sign-up and password-reset pages, which cannot
/// be submitted without it. (Signing in does not use it: `flectar-turnstile` has signin off.)
const FRAME_HOSTS: [&str; 2] = ["blizznux.com", "challenges.cloudflare.com"];

/// The launcher's own page: `tauri://localhost` from the bundled frontend, and the scheme Tauri
/// uses for it on the platforms where a custom scheme needs an http form. `about:blank` is what a
/// frame starts at before its first document.
fn own_page(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        "about" => url.path() == "blank",
        _ => false,
    }
}

/// The address the launcher's page is loaded from. Tauri serves `index.html` for the bare origin,
/// so this works after any navigation away.
pub(crate) const APP_PAGE: &str = "tauri://localhost/";

/// Whether a URI is the launcher's own page — the only document allowed in the top frame.
pub(crate) fn is_app_page(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|url| own_page(&url))
}

/// Decide what to do with a navigation request. Kept free of the webview so it can be exercised
/// without a window; the handlers below only carry the answer out.
///
/// The decision is per destination, not per frame: WebKitGTK 2.54 no longer hands the frame to a
/// navigation decision (`webkit_navigation_policy_decision_get_frame` is gone; only the frame name
/// remains), and the decision Tauri exposes arrives from every frame at once. The destination is
/// unambiguous in this app: only the bundled page is ever `tauri://localhost`, and the launcher's
/// own page never navigates the top frame to blizznux.com (the forum lives in the iframe, and its
/// links the launcher opens itself go through `open_external`). What the allow list lets through
/// can still replace the top document, so the top document is put back after the load; see
/// `is_app_page` and the `load-changed` guard in lib.rs.
pub(crate) fn navigation(uri: &str) -> Nav {
    let Ok(url) = Url::parse(uri) else { return Nav::Refuse };
    if own_page(&url) {
        return Nav::Allow;
    }
    match url.scheme() {
        "https" if url.port_or_known_default() == Some(443) => match url.host_str() {
            // Hosts compare case-insensitively, as DNS does; `Url` has already lowercased this one.
            Some(host) if FRAME_HOSTS.contains(&host) => Nav::Allow,
            _ => Nav::External,
        },
        "http" | "https" => Nav::External,
        // javascript:, data:, blob:, file:, mailto:, anything else: neither loaded nor opened.
        _ => Nav::Refuse,
    }
}

/// A link that asks for a new window (`target="_blank"`, `window.open`, a context-menu entry)
/// never gets one inside the launcher: any http/https address, the forum's own included, goes to
/// the browser; everything else is dropped.
pub(crate) fn new_window(uri: &str) -> Nav {
    let Ok(url) = Url::parse(uri) else { return Nav::Refuse };
    match url.scheme() {
        "http" | "https" => Nav::External,
        _ => Nav::Refuse,
    }
}

/// Hand an address to the user's browser. The host command keeps the AppImage's library and
/// Python environment out of it (see `host_command`); a browser that fails to start leaves the
/// navigation cancelled, which is the safe half of the outcome.
pub(crate) fn open_in_browser(uri: &str) {
    let Ok(url) = Url::parse(uri) else { return };
    if !matches!(url.scheme(), "http" | "https") {
        return;
    }
    let _ = crate::host_command("xdg-open")
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Tauri's navigation hook, which on Linux is WebKitGTK's `decide-policy` navigation decision:
/// it fires for the top frame and for every subframe, so the forum frame is covered by the same
/// rule as the page around it. Returning false cancels the navigation.
pub(crate) fn plugin() -> TauriPlugin<tauri::Wry> {
    Builder::new("nav-allowlist")
        .on_navigation(|_webview, url| match navigation(url.as_str()) {
            Nav::Allow => true,
            Nav::External => {
                open_in_browser(url.as_str());
                false
            }
            Nav::Refuse => false,
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_own_page_is_allowed() {
        for uri in [
            "tauri://localhost",
            "tauri://localhost/index.html",
            "tauri://localhost/index.html#settings",
            "http://tauri.localhost/",
            "https://tauri.localhost/index.html",
            "about:blank",
        ] {
            assert_eq!(navigation(uri), Nav::Allow, "{uri}");
        }
    }

    #[test]
    fn the_forum_stays_in_the_frame() {
        for uri in [
            "https://blizznux.com",
            "https://blizznux.com/",
            "https://blizznux.com/d/1-x?y=2#z",
            "https://blizznux.com/wiki",
            "https://blizznux.com/login",
            "https://blizznux.com/logout",
            "https://blizznux.com/assets/forum.js?v=ca7349b9",
            "https://blizznux.com:443/d/1-x",
            "https://user:pass@blizznux.com/d/1-x",
        ] {
            assert_eq!(navigation(uri), Nav::Allow, "{uri}");
        }
    }

    #[test]
    fn the_turnstile_frame_of_sign_up_and_reset_is_allowed() {
        // The site's sign-up and forgot-password pages embed Cloudflare's widget in a frame.
        assert_eq!(navigation("https://challenges.cloudflare.com/turnstile/v0/api.js"), Nav::Allow);
        assert_eq!(
            navigation("https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/g/turnstile/if/ov2/av0/x"),
            Nav::Allow
        );
    }

    #[test]
    fn everything_else_https_goes_to_the_browser() {
        for uri in [
            "https://evil.example/",
            "https://evil.example/d/1-x",
            "http://blizznux.com/",
            "http://blizznux.com/d/1-x?y=2#z",
            "https://blizznux.com.evil.example/",
            "https://evil.example/?next=https://blizznux.com/",
            "https://www.blizznux.com/",
            "https://status.blizznux.com/badge.svg",
            "https://blizznux.com:8443/d/1-x",
            "https://challenges.cloudflare.com.evil.example/",
        ] {
            assert_eq!(navigation(uri), Nav::External, "{uri}");
        }
    }

    #[test]
    fn other_schemes_are_refused_without_a_browser() {
        for uri in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "blob:https://blizznux.com/6f9b4c1e-0d5a-4a1e-9a0e-2c1f0e9a7b3d",
            "file:///etc/passwd",
            "mailto:someone@example.com",
            "vbscript:msgbox(1)",
            "tauri://elsewhere/index.html",
        ] {
            assert_eq!(navigation(uri), Nav::Refuse, "{uri}");
        }
    }

    #[test]
    fn an_odd_or_empty_uri_is_refused() {
        for uri in ["", " ", "   ", "://", "http://", "https://", "not a url", "\u{0}", "//blizznux.com"] {
            assert_eq!(navigation(uri), Nav::Refuse, "{uri:?}");
        }
    }

    #[test]
    fn host_case_is_ignored_the_way_dns_ignores_it() {
        // `Url` lowercases the host, so a link written in capitals is the same forum page.
        assert_eq!(navigation("https://BlizzNux.COM/d/1-x?y=2#z"), Nav::Allow);
        assert_eq!(navigation("HTTPS://BLIZZNUX.COM/"), Nav::Allow);
        // and the same trick cannot smuggle another host past the list
        assert_eq!(navigation("https://BlizzNux.COM.evil.example/"), Nav::External);
        assert_eq!(navigation("https://BlizzNux.COM@evil.example/"), Nav::External);
    }

    #[test]
    fn only_the_own_page_counts_as_the_top_document() {
        for uri in [
            "tauri://localhost",
            "tauri://localhost/",
            "http://tauri.localhost/",
            "https://tauri.localhost/x",
            "about:blank",
        ] {
            assert!(is_app_page(uri), "{uri}");
        }
        for uri in [
            "https://blizznux.com/",
            "https://challenges.cloudflare.com/turnstile/v0/api.js",
            "https://evil.example/",
            "https://tauri.localhost.evil.example/",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!is_app_page(uri), "{uri}");
        }
    }

    #[test]
    fn a_new_window_request_never_stays_in_the_launcher() {
        for uri in [
            "https://blizznux.com/d/1-x",
            "https://blizznux.com/wiki",
            "https://evil.example/",
            "http://blizznux.com/",
            "https://blizznux.com.evil.example/",
        ] {
            assert_eq!(new_window(uri), Nav::External, "{uri}");
        }
        for uri in [
            "about:blank",
            "tauri://localhost/index.html",
            "javascript:alert(1)",
            "data:text/html,x",
            "blob:https://blizznux.com/1",
            "file:///etc/passwd",
            "",
        ] {
            assert_eq!(new_window(uri), Nav::Refuse, "{uri}");
        }
    }
}
