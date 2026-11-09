use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, uri::Authority, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AppState;

use localreel_media::{CurrentUser, Role};

/// Decide, for one request, whether error bodies may carry diagnostics.
///
/// A diagnostic is a local path or an encoder's stderr: useful while
/// troubleshooting, never appropriate for a viewer. It is therefore revealed
/// only to an administrator, and only from loopback or when `RUST_LOG` asks for
/// debug output. Everyone else gets the stable code and message.
///
/// Runs *inside* [`crate::auth::authenticate`] so the session is already on the
/// request extensions.
pub async fn detail_scope(request: Request, next: Next) -> Response {
    let admin = request
        .extensions()
        .get::<CurrentUser>()
        .is_some_and(|current| current.user.role == Role::Admin);
    let loopback = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_loopback_authority);
    let reveal = should_reveal_detail(admin, loopback, crate::error::debug_logging_enabled());
    crate::error::with_reveal_detail(reveal, next.run(request)).await
}

/// Diagnostics are for administrators, from loopback or while debugging.
fn should_reveal_detail(admin: bool, loopback: bool, debug: bool) -> bool {
    admin && (loopback || debug)
}

/// What the guard decides for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteVerdict {
    Allow,
    Forbidden,
}

/// The `host[:port]` inside an origin, e.g. `http://nas.local:3101` → `nas.local:3101`.
fn origin_authority(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    Some(rest.split('/').next().unwrap_or(rest))
}

fn same_authority(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Whether a request may proceed.
///
/// With remote access off, only loopback authorities are accepted — the
/// DNS-rebinding guard LocalReel has always had. With it on, a non-loopback
/// `Host` is accepted, but a browser that sends an `Origin` must be talking to
/// that same authority (or one the configuration names). Without that rule an
/// enabled LAN address would also accept requests driven by any other page the
/// household happens to have open.
///
/// Requests without an `Origin` (curl, a top-level navigation) are allowed:
/// a cookie-less request gains nothing, and the session check is downstream.
pub fn remote_verdict(
    host: Option<&str>,
    origin: Option<&str>,
    allow_remote: bool,
    allowed_origins: &[String],
) -> RemoteVerdict {
    let Some(host) = host else {
        return RemoteVerdict::Forbidden;
    };
    let host_is_loopback = is_loopback_authority(host);

    if !allow_remote {
        let host_ok = host_is_loopback;
        let origin_ok = origin.map(is_loopback_origin).unwrap_or(true);
        return if host_ok && origin_ok {
            RemoteVerdict::Allow
        } else {
            RemoteVerdict::Forbidden
        };
    }

    let Some(origin) = origin else {
        return RemoteVerdict::Allow;
    };
    let authority = origin_authority(origin).unwrap_or_default();
    let matches_host = same_authority(authority, host);
    let listed = allowed_origins
        .iter()
        .any(|allowed| same_authority(allowed.trim_end_matches('/'), origin.trim_end_matches('/')));
    if matches_host || listed {
        RemoteVerdict::Allow
    } else {
        RemoteVerdict::Forbidden
    }
}

/// Default local-only guard.
///
/// Rejects requests whose `Host` is not loopback, and (when present) whose
/// `Origin` is not loopback. This is the DNS-rebinding/cross-site guard used by
/// the previous LocalReel app; it is not an authorization boundary. Remote
/// access must be explicitly enabled with `allow_remote`, and even then the
/// origin has to agree with the host (see [`remote_verdict`]).
pub async fn loopback_guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());

    let verdict = remote_verdict(
        host,
        origin,
        state.config.allow_remote,
        &state.config.allowed_origins,
    );

    if verdict == RemoteVerdict::Allow {
        return next.run(request).await;
    }

    let message = if state.config.allow_remote {
        "LocalReel accepts requests only from its own address"
    } else {
        "LocalReel only accepts loopback requests"
    };
    (StatusCode::FORBIDDEN, message).into_response()
}

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
    script-src 'self'; style-src 'self'; img-src 'self' data:; \
    media-src 'self' blob:; connect-src 'self'; font-src 'self'; \
    object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Add conservative browser security headers to every response. The UI has no
/// inline scripts/styles and loads everything from the same origin.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    response
}

fn is_loopback_authority(value: &str) -> bool {
    let Ok(authority) = value.parse::<Authority>() else {
        return false;
    };
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

fn is_loopback_origin(value: &str) -> bool {
    let Ok(uri) = value.parse::<axum::http::Uri>() else {
        return false;
    };
    uri.authority()
        .map(|authority| is_loopback_authority(authority.as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_authorities_are_accepted() {
        assert!(is_loopback_authority("127.0.0.1:3001"));
        assert!(is_loopback_authority("localhost:5173"));
        assert!(is_loopback_authority("[::1]:3001"));
        assert!(!is_loopback_authority("0.0.0.0:3001"));
        assert!(!is_loopback_authority("example.com:3001"));
    }

    #[test]
    fn loopback_origins_are_accepted() {
        assert!(is_loopback_origin("http://127.0.0.1:5173"));
        assert!(is_loopback_origin("http://localhost:5173"));
        assert!(!is_loopback_origin("https://example.com"));
        assert!(!is_loopback_origin("null"));
    }

    #[test]
    fn remote_access_off_keeps_everything_local() {
        assert_eq!(
            remote_verdict(Some("127.0.0.1:3101"), None, false, &[]),
            RemoteVerdict::Allow
        );
        assert_eq!(
            remote_verdict(Some("192.168.1.10:3101"), None, false, &[]),
            RemoteVerdict::Forbidden
        );
        // A local page driven by a remote origin is still refused.
        assert_eq!(
            remote_verdict(
                Some("127.0.0.1:3101"),
                Some("https://evil.example"),
                false,
                &[]
            ),
            RemoteVerdict::Forbidden
        );
    }

    #[test]
    fn remote_access_on_accepts_the_lan_address_and_its_own_origin() {
        assert_eq!(
            remote_verdict(Some("192.168.1.10:3101"), None, true, &[]),
            RemoteVerdict::Allow
        );
        assert_eq!(
            remote_verdict(
                Some("192.168.1.10:3101"),
                Some("http://192.168.1.10:3101"),
                true,
                &[]
            ),
            RemoteVerdict::Allow
        );
        // A different page on the same network may not drive the session.
        assert_eq!(
            remote_verdict(
                Some("192.168.1.10:3101"),
                Some("http://192.168.1.55:8080"),
                true,
                &[]
            ),
            RemoteVerdict::Forbidden
        );
    }

    #[test]
    fn a_configured_origin_is_accepted_behind_a_reverse_proxy() {
        let allowed = vec!["https://reels.example.com".to_string()];
        assert_eq!(
            remote_verdict(
                Some("127.0.0.1:3101"),
                Some("https://reels.example.com"),
                true,
                &allowed
            ),
            RemoteVerdict::Allow
        );
        assert_eq!(
            remote_verdict(
                Some("127.0.0.1:3101"),
                Some("https://other.example.com"),
                true,
                &allowed
            ),
            RemoteVerdict::Forbidden
        );
        // Trailing slashes and case are not a reason to refuse.
        assert_eq!(
            remote_verdict(
                Some("127.0.0.1:3101"),
                Some("https://Reels.Example.com/"),
                true,
                &allowed
            ),
            RemoteVerdict::Allow
        );
    }

    #[test]
    fn a_request_without_a_host_header_is_refused() {
        assert_eq!(
            remote_verdict(None, None, true, &[]),
            RemoteVerdict::Forbidden
        );
        assert_eq!(
            remote_verdict(None, None, false, &[]),
            RemoteVerdict::Forbidden
        );
    }

    #[test]
    fn diagnostics_are_only_revealed_to_a_local_or_debugging_administrator() {
        // The ordinary viewer never sees an internal path, whatever the mode.
        assert!(!should_reveal_detail(false, true, false));
        assert!(!should_reveal_detail(false, false, true));
        // An administrator on this machine, or one troubleshooting.
        assert!(should_reveal_detail(true, true, false));
        assert!(should_reveal_detail(true, true, true));
        // An administrator over the network only while debug logging is on.
        assert!(should_reveal_detail(true, false, true));
        assert!(!should_reveal_detail(true, false, false));
    }
}
