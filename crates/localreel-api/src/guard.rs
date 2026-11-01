use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, uri::Authority, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AppState;

/// Default local-only guard.
///
/// Rejects requests whose `Host` is not loopback, and (when present) whose
/// `Origin` is not loopback. This is the DNS-rebinding/cross-site guard used by
/// the previous LocalReel app; it is not an authorization boundary. Remote
/// access must be explicitly enabled with `LOCALREEL_ALLOW_REMOTE=1`.
pub async fn loopback_guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if state.config.allow_remote {
        return next.run(request).await;
    }

    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_loopback_authority);
    let origin_ok = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(is_loopback_origin)
        .unwrap_or(true);

    if !host_ok || !origin_ok {
        return (
            StatusCode::FORBIDDEN,
            "LocalReel only accepts loopback requests",
        )
            .into_response();
    }

    next.run(request).await
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
}
