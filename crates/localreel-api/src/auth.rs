//! Authentication: session cookies, CSRF enforcement and account management.
//!
//! Every `/api/v1` route except health and the auth endpoints itself requires a
//! session. The [`authenticate`] middleware resolves the session cookie, stores
//! the [`CurrentUser`] in the request extensions and rejects state-changing
//! requests that do not echo the CSRF token.

use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use localreel_media::{CurrentUser, Role, User, UserPatch};

use crate::error::ApiError;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "localreel_session";
pub const CSRF_COOKIE: &str = "localreel_csrf";
pub const CSRF_HEADER: &str = "x-localreel-csrf";

/// Paths that work without a session: the health probe (launchers use it) and
/// the auth endpoints themselves (the login page needs them).
const PUBLIC_PATHS: &[&str] = &["/api/v1/health", "/api/v1/auth/"];

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub setup_required: bool,
    pub authenticated: bool,
    pub user: Option<User>,
    pub mode: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub user: User,
    pub csrf: String,
}

/// Login and first-run setup.
pub async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let setup_required = state.accounts.setup_required().unwrap_or(false);
    let user = current_user(&state, &headers);
    Json(StatusResponse {
        setup_required,
        authenticated: user.is_some(),
        user: user.map(|current| current.user),
        mode: state.config.mode.as_str(),
    })
    .into_response()
}

pub async fn setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    if !state.accounts.setup_required()? {
        return Err(ApiError::conflict(
            "the owner account is already configured",
        ));
    }
    let user = state.accounts.setup_owner(&body.username, &body.password)?;
    let session = state.accounts.create_session(user.id)?;
    Ok(login_response(
        user,
        session.csrf,
        session.id,
        is_secure(&headers),
    ))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    state.login_limiter.check(peer.ip())?;
    match state
        .accounts
        .authenticate(&body.username, &body.password)?
    {
        Some(user) => {
            state.login_limiter.clear(peer.ip());
            let session = state.accounts.create_session(user.id)?;
            Ok(login_response(
                user,
                session.csrf,
                session.id,
                is_secure(&headers),
            ))
        }
        None => {
            state.login_limiter.record_failure(peer.ip());
            Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid username or password",
            ))
        }
    }
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie(&headers, SESSION_COOKIE) {
        let _ = state.accounts.end_session(&token);
    }
    clear_session_response()
}

pub async fn me(Extension(current): Extension<CurrentUser>) -> Json<User> {
    Json(current.user)
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub password: Option<String>,
}

pub async fn list_users(State(state): State<Arc<AppState>>) -> Result<Json<Vec<User>>, ApiError> {
    Ok(Json(state.accounts.list_users()?))
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateUserRequest>,
) -> Result<Json<User>, ApiError> {
    let role = match body.role.as_deref() {
        None => Role::User,
        Some("admin") => Role::Admin,
        Some("user") => Role::User,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown role '{other}': expected 'admin' or 'user'"
            )))
        }
    };
    let user = state
        .accounts
        .create_user(&body.username, &body.password, role)?;
    Ok(Json(user))
}

pub async fn update_user(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Extension(current): Extension<CurrentUser>,
    Json(body): Json<UpdateUserRequest>,
) -> Result<Json<User>, ApiError> {
    if id == current.user.id && body.enabled == Some(false) {
        return Err(ApiError::bad_request("you cannot disable your own account"));
    }
    let role = body.role.as_deref().map(Role::parse).transpose()?;
    let patch = UserPatch {
        role,
        enabled: body.enabled,
        password: body.password,
    };
    Ok(Json(state.accounts.update_user(id, patch)?))
}

pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Extension(current): Extension<CurrentUser>,
) -> Result<StatusCode, ApiError> {
    if id == current.user.id {
        return Err(ApiError::bad_request("you cannot delete your own account"));
    }
    state.accounts.delete_user(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve the session cookie and enforce CSRF for state-changing requests.
pub async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    if PUBLIC_PATHS
        .iter()
        .any(|public| path == *public || path.starts_with(public))
    {
        return next.run(request).await;
    }

    let Some(token) = cookie(request.headers(), SESSION_COOKIE) else {
        return unauthorized("sign in to continue");
    };
    let current = match state.accounts.session(&token) {
        Ok(Some(current)) => current,
        Ok(None) => return unauthorized("your session has expired; sign in again"),
        Err(error) => return ApiError::from(error).into_response(),
    };

    let method = request.method().clone();
    if requires_csrf(&method) {
        let presented = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if presented.is_empty() || presented != current.csrf {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "csrf",
                "missing or invalid CSRF token",
            )
            .into_response();
        }
    }

    request.extensions_mut().insert(current);
    next.run(request).await
}

/// Reject non-administrators from the admin route group.
pub async fn require_admin(request: Request, next: Next) -> Response {
    let is_admin = request
        .extensions()
        .get::<CurrentUser>()
        .is_some_and(|current| current.user.is_admin());
    if !is_admin {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrator role required",
        )
        .into_response();
    }
    next.run(request).await
}

fn current_user(state: &AppState, headers: &HeaderMap) -> Option<CurrentUser> {
    let token = cookie(headers, SESSION_COOKIE)?;
    state.accounts.session(&token).ok().flatten()
}

fn requires_csrf(method: &Method) -> bool {
    !matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

/// Read one cookie value from the request headers.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(raw) = value.to_str() else { continue };
        for pair in raw.split(';') {
            let pair = pair.trim();
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if key.trim() == name {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn login_response(user: User, csrf: String, session: String, secure: bool) -> Response {
    let mut response = Json(SessionResponse {
        user,
        csrf: csrf.clone(),
    })
    .into_response();
    let ttl = localreel_media::account::SESSION_TTL_SECS;
    let mut cookies = Vec::new();
    cookies.push(format!(
        "{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{}",
        if secure { "; Secure" } else { "" }
    ));
    cookies.push(format!(
        "{CSRF_COOKIE}={csrf}; Path=/; SameSite=Lax; Max-Age={ttl}{}",
        if secure { "; Secure" } else { "" }
    ));
    for value in cookies {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

fn clear_session_response() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    for name in [SESSION_COOKIE, CSRF_COOKIE] {
        let value = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

fn unauthorized(message: &str) -> Response {
    ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", message).into_response()
}

/// Keeps brute-force attempts against a LAN-visible server bounded.
#[derive(Debug, Default)]
pub struct LoginLimiter {
    entries: std::sync::Mutex<std::collections::HashMap<IpAddr, Attempt>>,
}

#[derive(Debug, Clone, Copy)]
struct Attempt {
    failures: u32,
    blocked_until: i64,
    window_start: i64,
}

/// Failures inside this window count towards a block.
const WINDOW_SECS: i64 = 15 * 60;
/// Consecutive failures that trigger a block.
const MAX_FAILURES: u32 = 10;
/// How long a blocked address must wait.
const BLOCK_SECS: i64 = 60;

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(&self, ip: IpAddr) -> Result<(), ApiError> {
        let now = localreel_kernel::time::now_secs();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(attempt) = entries.get_mut(&ip) {
            if attempt.blocked_until > now {
                let wait = attempt.blocked_until - now;
                return Err(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                    format!("too many failed sign-ins; try again in {wait}s"),
                ));
            }
            if attempt.blocked_until != 0 && attempt.blocked_until <= now {
                attempt.failures = 0;
                attempt.blocked_until = 0;
                attempt.window_start = now;
            }
            if now - attempt.window_start > WINDOW_SECS {
                attempt.failures = 0;
                attempt.window_start = now;
            }
        }
        Ok(())
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let now = localreel_kernel::time::now_secs();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let attempt = entries.entry(ip).or_insert(Attempt {
            failures: 0,
            blocked_until: 0,
            window_start: now,
        });
        attempt.failures += 1;
        if attempt.failures >= MAX_FAILURES {
            attempt.blocked_until = now + BLOCK_SECS;
        }
    }

    pub fn clear(&self, ip: IpAddr) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.remove(&ip);
    }
}

fn is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn reads_a_cookie_from_a_combined_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; localreel_session=abc123; b=2"),
        );
        assert_eq!(
            cookie(&headers, "localreel_session").as_deref(),
            Some("abc123")
        );
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[test]
    fn only_mutating_methods_need_a_csrf_token() {
        assert!(!requires_csrf(&Method::GET));
        assert!(!requires_csrf(&Method::HEAD));
        assert!(requires_csrf(&Method::POST));
        assert!(requires_csrf(&Method::PUT));
        assert!(requires_csrf(&Method::PATCH));
        assert!(requires_csrf(&Method::DELETE));
    }

    #[test]
    fn repeated_failures_block_an_address() {
        let limiter = LoginLimiter::new();
        let ip = "192.168.1.20".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            assert!(limiter.check(ip).is_ok());
            limiter.record_failure(ip);
        }
        assert!(limiter.check(ip).is_err());
        limiter.clear(ip);
        assert!(limiter.check(ip).is_ok());
    }
}
