use std::convert::Infallible;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::{delete, get, patch, post},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tower_http::services::ServeDir;

use localreel_kernel::jobs::JobRecord;
use localreel_kernel::storage::{MediaUri, Storage};
use localreel_kernel::time::now_secs;
use localreel_media::playback::{direct_play_supported, is_hls, preferred_preset};
use localreel_media::{
    BackupImportReport, CacheGcReport, CacheStats, CurrentUser, DeleteSourceReport, FeedItem,
    Media, MediaFilter, MediaState, RemapReport, ScanIssue, ScanRun, Source,
};
use localreel_plugin::AuditEvent;

use crate::auth;
use crate::error::ApiError;
use crate::guard::{detail_scope, loopback_guard, security_headers};
use crate::state::AppState;

const MAX_RANGE_CHUNK: u64 = 8 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let thumbnails = ServeDir::new(state.config.media.thumbnail_dir.clone());
    let transcodes = ServeDir::new(state.config.media.transcode_dir.clone());
    let previews = ServeDir::new(state.config.media.preview_dir.clone());
    let posters = ServeDir::new(state.config.media.poster_dir.clone());
    let embedded = crate::web::is_embedded();
    let web_dir = state.config.resolved_web_dir();

    // Reachable without a session: the launcher's health probe and the sign-in
    // endpoints themselves.
    let public_api = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/auth/status", get(auth::status))
        .route("/api/v1/auth/setup", post(auth::setup))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/logout", post(auth::logout))
        // Plugins have no browser session; this one authenticates with the
        // token the host gave the sidecar when it started it.
        .route("/api/v1/plugin-proxy", post(plugin_proxy));

    // Any signed-in account: the shared library plus that account's own state.
    let user_api = Router::new()
        .route("/api/v1/auth/me", get(auth::me))
        .route("/api/v1/media", get(list_media))
        .route("/api/v1/feed", get(feed))
        .route("/api/v1/search", get(search_media))
        .route("/api/v1/media/:id", get(get_media))
        .route("/api/v1/media/:id/tracks", get(list_tracks))
        .route("/api/v1/media/:id/state", get(get_state).put(put_state))
        .route("/api/v1/media/:id/playback", post(prepare_playback))
        .route("/api/v1/media/:id/preview.vtt", get(preview_vtt))
        .route(
            "/api/v1/media/:id/stream",
            get(stream_media).head(head_media),
        )
        .route("/api/v1/events", get(events))
        .nest_service("/thumbnails", thumbnails)
        .nest_service("/transcodes", transcodes)
        .nest_service("/previews", previews)
        .nest_service("/posters", posters)
        .route("/hls/*path", get(serve_hls));

    // Administrators only: library structure, jobs, backups and accounts.
    let admin_api = Router::new()
        .route("/api/v1/admin/health", get(admin_health))
        .route("/api/v1/admin/update", get(admin_update))
        .route("/api/v1/admin/shutdown", post(shutdown_server))
        .route("/api/v1/admin/cache", get(cache_stats))
        .route("/api/v1/admin/cache/gc", post(run_cache_gc))
        .route("/api/v1/plugins", get(list_plugins))
        .route("/api/v1/plugins/audit", get(plugin_audit))
        .route(
            "/api/v1/plugins/:id/capabilities",
            get(plugin_capabilities).post(grant_plugin_capability),
        )
        .route(
            "/api/v1/plugins/:id/capabilities/:capability",
            delete(revoke_plugin_capability),
        )
        .route("/api/v1/plugins/:id/restart", post(plugin_restart))
        .route("/api/v1/plugins/:id/enable", post(plugin_enable))
        .route("/api/v1/plugins/:id/disable", post(plugin_disable))
        .route("/api/v1/plugins/:id/logs", get(plugin_logs))
        .route("/api/v1/metadata", get(metadata_status))
        .route("/api/v1/metadata/match", post(match_metadata))
        .route("/api/v1/backup", get(download_backup).post(upload_backup))
        .route("/api/v1/backup/restore", post(enqueue_restore))
        .route("/api/v1/backup/restore/upload", post(upload_restore_file))
        .route("/api/v1/scan-runs", get(list_scan_runs))
        .route("/api/v1/scan-issues", get(list_scan_issues))
        .route("/api/v1/sources", get(list_sources).post(add_source))
        .route(
            "/api/v1/sources/:id",
            patch(patch_source).delete(delete_source),
        )
        .route("/api/v1/sources/:id/scan", post(scan_source))
        .route("/api/v1/sources/:id/remap", post(remap_source))
        .route("/api/v1/jobs", get(list_jobs))
        .route("/api/v1/jobs/:id", get(get_job))
        .route("/api/v1/jobs/:id/cancel", post(cancel_job))
        .route(
            "/api/v1/users",
            get(auth::list_users).post(auth::create_user),
        )
        .route(
            "/api/v1/users/:id",
            patch(auth::update_user).delete(auth::delete_user),
        )
        .route_layer(axum::middleware::from_fn(auth::require_admin));

    let api = public_api
        .merge(user_api)
        .merge(admin_api)
        .layer(axum::middleware::from_fn(detail_scope))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ));

    let mut router = Router::new().merge(api);

    if embedded {
        router = router
            .route("/", get(embedded_index))
            .route("/admin", get(embedded_index))
            .route("/watch", get(embedded_index))
            .route("/assets/*path", get(embedded_asset))
            .route("/manifest.webmanifest", get(embedded_manifest))
            .route("/favicon.ico", get(embedded_favicon))
            .route("/icons/*path", get(embedded_public_file));
    } else {
        router = router
            .route("/", get(index))
            .route("/admin", get(index))
            .route("/watch", get(index))
            .route("/manifest.webmanifest", get(embedded_manifest))
            .route("/favicon.ico", get(embedded_favicon))
            .route("/icons/*path", get(embedded_public_file));
        if let Some(web_dir) = web_dir {
            router = router.nest_service("/assets", ServeDir::new(web_dir.join("assets")));
        }
    }

    router
        .layer(DefaultBodyLimit::max(
            usize::try_from(localreel_media::MAX_BACKUP_BYTES).unwrap_or(32 * 1024 * 1024),
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            loopback_guard,
        ))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

async fn embedded_index() -> Response {
    match crate::web::index() {
        Some((bytes, mime)) => bytes_response(bytes, mime, "no-cache"),
        None => Html(INDEX_HTML).into_response(),
    }
}

async fn embedded_asset(Path(path): Path<String>) -> Response {
    let asset = format!("assets/{path}");
    match crate::web::get(&asset) {
        Some((bytes, mime)) => bytes_response(bytes, mime, "public, max-age=31536000, immutable"),
        None => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}

/// Root-level files the browser requests by fixed name (`manifest.webmanifest`,
/// `favicon.ico`). Without these a packaged build 404s and the UI cannot be
/// installed as a PWA even though the file is embedded.
async fn root_file(state: &AppState, name: &str) -> Response {
    if let Some((bytes, mime)) = crate::web::get(name) {
        return bytes_response(bytes, mime, "public, max-age=3600");
    }
    if let Some(web_dir) = state.config.resolved_web_dir() {
        if let Ok(bytes) = tokio::fs::read(web_dir.join(name)).await {
            let mime = mime_guess::from_path(name)
                .first_or_octet_stream()
                .to_string();
            return bytes_response_owned(bytes, mime, "public, max-age=3600");
        }
    }
    (StatusCode::NOT_FOUND, "file not found").into_response()
}

async fn embedded_manifest(State(state): State<Arc<AppState>>) -> Response {
    root_file(&state, "manifest.webmanifest").await
}

async fn embedded_favicon(State(state): State<Arc<AppState>>) -> Response {
    root_file(&state, "favicon.ico").await
}

/// Files under `public/` subdirectories (currently `icons/`).
async fn embedded_public_file(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    let asset = format!("public/{path}");
    let nested = format!("icons/{path}");
    for candidate in [asset.as_str(), nested.as_str()] {
        if let Some((bytes, mime)) = crate::web::get(candidate) {
            return bytes_response(bytes, mime, "public, max-age=86400");
        }
    }
    if let Some(web_dir) = state.config.resolved_web_dir() {
        for candidate in [asset.as_str(), nested.as_str()] {
            if let Ok(bytes) = tokio::fs::read(web_dir.join(candidate)).await {
                let mime = mime_guess::from_path(candidate)
                    .first_or_octet_stream()
                    .to_string();
                return bytes_response_owned(bytes, mime, "public, max-age=86400");
            }
        }
    }
    (StatusCode::NOT_FOUND, "file not found").into_response()
}

fn bytes_response_owned(bytes: Vec<u8>, mime: String, cache: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn bytes_response(bytes: &'static [u8], mime: String, cache: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(bytes.to_vec()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    if let Some(web_dir) = state.config.resolved_web_dir() {
        match tokio::fs::read_to_string(web_dir.join("index.html")).await {
            Ok(html) => return Html(html).into_response(),
            Err(error) => tracing::warn!("failed to read web index: {error}"),
        }
    }
    Html(INDEX_HTML).into_response()
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    /// Stable identifier, so launchers can tell LocalReel apart from anything
    /// else that happens to listen on the configured port.
    name: &'static str,
    status: &'static str,
    version: &'static str,
    mode: &'static str,
    database: bool,
    ffmpeg: bool,
    /// `full`, `probe-only` or `missing`; `ffmpeg` stays the "can transcode"
    /// boolean so existing launchers keep working.
    ffmpeg_capability: &'static str,
    media_count: i64,
    uptime_seconds: i64,
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let database = state.db.health_check().is_ok();
    let media_count = state.media.media_count().unwrap_or(0);
    Json(HealthResponse {
        name: "LocalReel",
        status: if database { "ok" } else { "degraded" },
        version: localreel_kernel::VERSION,
        mode: state.config.mode.as_str(),
        database,
        ffmpeg: state.ffmpeg_capability.can_transcode(),
        ffmpeg_capability: state.ffmpeg_capability.as_str(),
        media_count,
        uptime_seconds: (now_secs() - state.started_at).max(0),
    })
}

/// Ask the process to stop. Administrators only: this is the desktop escape
/// hatch on platforms without a tray icon, and it uses the same graceful path
/// as Ctrl+C, so in-flight requests still finish.
async fn shutdown_server(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    // The server logs the shutdown itself; this only raises the signal.
    state.shutdown.notify_one();
    Json(serde_json::json!({ "status": "shutting-down" }))
}

#[derive(Debug, Deserialize)]
struct ProxyRequest {
    url: String,
}

/// Outbound HTTP on behalf of a plugin.
///
/// The only sanctioned way for a sidecar to reach the network: it must present
/// the token the host gave it, it must have declared `network`, and every call
/// is audited. A plugin without the permission gets 403 and an audit row.
async fn plugin_proxy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ProxyRequest>,
) -> Result<Response, ApiError> {
    let token = headers
        .get("x-localreel-plugin")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if token.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "the x-localreel-plugin token header is required",
        ));
    }
    let proxied = state.plugins.proxy(token, &payload.url)?;
    let status = StatusCode::from_u16(proxied.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = proxied.content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder
        .body(Body::from(proxied.body))
        .map_err(|error| ApiError::internal(error.to_string()))
}

/// Installed plugins with the permissions that actually apply to them.
async fn list_plugins(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "plugins": state.plugins.summaries() }))
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    plugin: Option<String>,
    limit: Option<i64>,
}

/// Recent plugin audit entries, newest first.
async fn plugin_audit(
    State(state): State<Arc<AppState>>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let entries = state
        .plugins
        .audit()
        .map(|audit| audit.recent(query.plugin.as_deref(), limit))
        .transpose()?
        .unwrap_or_default();
    Ok(Json(serde_json::json!({ "entries": entries })))
}

/// What a plugin asks for, what it was granted, and the host's own risk text.
async fn plugin_capabilities(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let report = state
        .plugins
        .capability_report(&id)
        .ok_or_else(|| ApiError::not_found(format!("plugin '{id}'")))?;
    let body = serde_json::to_value(report)
        .map_err(|error| ApiError::internal(format!("failed to encode capabilities: {error}")))?;
    Ok(Json(body))
}

fn empty_capability_scope() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, Deserialize)]
struct GrantCapabilityRequest {
    capability: String,
    #[serde(default = "empty_capability_scope")]
    scope: serde_json::Value,
    expires_at: Option<i64>,
}

/// Grant one capability for the plugin's current version.
async fn grant_plugin_capability(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(current): Extension<CurrentUser>,
    Json(body): Json<GrantCapabilityRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let descriptor = state
        .plugins
        .descriptor(&id)
        .ok_or_else(|| ApiError::not_found(format!("plugin '{id}'")))?;
    if body.capability.trim().is_empty() {
        return Err(ApiError::bad_request("capability must not be empty"));
    }
    if body.expires_at.is_some_and(|expires| expires <= now_secs()) {
        return Err(ApiError::bad_request("expires_at must be in the future"));
    }
    let grants = state.plugins.grants().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "capability grants are not available",
        )
    })?;
    let grant = grants.grant(
        &id,
        &descriptor.version,
        &body.capability,
        body.scope,
        Some(current.user.id),
        body.expires_at,
    )?;
    if let Some(audit) = state.plugins.audit() {
        let _ = audit.record_event(&AuditEvent {
            plugin_id: &id,
            capability: Some(&grant.capability),
            action: "capability.grant",
            result: "granted",
            detail: Some("granted by administrator"),
            scope_digest: Some(&grant.scope_digest),
            bytes_in: None,
            bytes_out: None,
            duration_ms: None,
            request_id: None,
            job_id: None,
            user_id: Some(current.user.id),
        });
    }
    let body = serde_json::to_value(grant)
        .map_err(|error| ApiError::internal(format!("failed to encode grant: {error}")))?;
    Ok(Json(body))
}

/// Revoke one capability for the plugin's current version.
async fn revoke_plugin_capability(
    State(state): State<Arc<AppState>>,
    Path((id, capability)): Path<(String, String)>,
    Extension(current): Extension<CurrentUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let descriptor = state
        .plugins
        .descriptor(&id)
        .ok_or_else(|| ApiError::not_found(format!("plugin '{id}'")))?;
    let grants = state.plugins.grants().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "capability grants are not available",
        )
    })?;
    let revoked = grants.revoke(&id, &descriptor.version, &capability)?;
    if let Some(audit) = state.plugins.audit() {
        let _ = audit.record_event(&AuditEvent {
            plugin_id: &id,
            capability: Some(&capability),
            action: "capability.revoke",
            result: if revoked { "revoked" } else { "not_found" },
            detail: Some("revoked by administrator"),
            scope_digest: None,
            bytes_in: None,
            bytes_out: None,
            duration_ms: None,
            request_id: None,
            job_id: None,
            user_id: Some(current.user.id),
        });
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "plugin": id,
        "capability": capability,
        "revoked": revoked,
    })))
}

#[derive(Debug, Deserialize)]
struct PluginControlQuery {
    /// Also rewrite `enabled` in the configuration file. Off by default: a
    /// click in the UI must not silently edit a file the operator maintains.
    #[serde(default)]
    persist: bool,
}

#[derive(Debug, Deserialize)]
struct PluginLogsQuery {
    bytes: Option<usize>,
}

/// Restart one plugin: stop its generation, then start a fresh one.
async fn plugin_restart(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.plugins.restart(&id)?;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

/// Turn a plugin on at runtime, optionally persisting the switch.
async fn plugin_enable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<PluginControlQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let persisted = control_plugin(&state, &id, true, query.persist)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "state": state.plugins.descriptor(&id).map(|d| d.state.as_str()),
        "persisted": persisted,
    })))
}

/// Turn a plugin off at runtime, optionally persisting the switch.
async fn plugin_disable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<PluginControlQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let persisted = control_plugin(&state, &id, false, query.persist)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "state": state.plugins.descriptor(&id).map(|d| d.state.as_str()),
        "persisted": persisted,
    })))
}

fn control_plugin(
    state: &AppState,
    id: &str,
    enabled: bool,
    persist: bool,
) -> Result<Option<bool>, ApiError> {
    let package = state.plugins.package_of(id);
    state.plugins.set_enabled(id, enabled)?;
    if !persist {
        return Ok(None);
    }
    let (Some(config_path), Some(package)) = (&state.config_source, package) else {
        // No configuration file to write (environment-only setup) or no
        // package to match: the runtime switch still applies.
        return Ok(Some(false));
    };
    Ok(Some(localreel_kernel::config::set_plugin_enabled(
        config_path,
        &package,
        enabled,
    )?))
}

/// The tail of a plugin's stderr, for the admin diagnostics view.
async fn plugin_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<PluginLogsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.plugins.descriptor(&id).is_none() {
        return Err(ApiError::not_found(format!("plugin '{id}'")));
    }
    let bytes = query.bytes.unwrap_or(8192).clamp(1, 64 * 1024);
    let log = state.plugins.logs(&id, bytes).unwrap_or_default();
    Ok(Json(serde_json::json!({ "id": id, "log": log })))
}

async fn admin_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let database = state.db.health_check().is_ok();
    let media_count = state.media.media_count().unwrap_or(0);
    let recent_jobs = state.jobs.list_recent(20)?;
    let queued_transcodes = state
        .jobs
        .count_active(localreel_media::gate::LIMITED_KINDS)
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "name": "LocalReel",
        "status": if database { "ok" } else { "degraded" },
        "version": localreel_kernel::VERSION,
        "mode": state.config.mode.as_str(),
        "database": database,
        "ffmpeg": state.ffmpeg_capability.can_transcode(),
        "ffmpeg_capability": state.ffmpeg_capability.as_str(),
        "hwaccel": state.hwaccel.as_str(),
        "media_count": media_count,
        "data_dir": state.config.data_dir,
        "thumbnail_dir": state.config.media.thumbnail_dir,
        "transcode_dir": state.config.media.transcode_dir,
        "watchers": state.watchers.status(),
        "transcodes": state
            .transcodes
            .status(usize::try_from(queued_transcodes).unwrap_or(0)),
        "jobs": recent_jobs,
    })))
}

#[derive(Debug, Deserialize)]
struct UpdateQuery {
    /// Bypass the cache interval when the operator asks explicitly.
    refresh: Option<bool>,
}

/// Latest published release, for the admin "update" card.
///
/// Read-only and rate-limited by the interval in `[update]`; the download is a
/// link to the release page, never something the server runs itself.
async fn admin_update(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UpdateQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let current = localreel_kernel::VERSION;
    let config = state.config.update.clone();
    if !config.check {
        return Ok(Json(
            crate::update::Status::build(false, current, None, None).to_json(),
        ));
    }

    let interval = config.interval_hours.saturating_mul(3600) as i64;
    let now = localreel_kernel::time::now_secs();
    let cached = state
        .update_cache
        .lock()
        .map(|cache| cache.clone())
        .unwrap_or(None);
    let stale = cached
        .as_ref()
        .map(|check| now - check.checked_at >= interval)
        .unwrap_or(true);

    let mut error = None;
    if query.refresh.unwrap_or(false) || stale {
        let feed = config
            .feed
            .clone()
            .unwrap_or_else(|| crate::update::DEFAULT_FEED.to_string());
        match tokio::task::spawn_blocking(move || crate::update::fetch(&feed)).await {
            Ok(Ok(release)) => {
                if let Ok(mut cache) = state.update_cache.lock() {
                    *cache = Some(crate::update::CachedCheck {
                        checked_at: now,
                        release,
                    });
                }
            }
            Ok(Err(message)) => error = Some(message),
            Err(join) => error = Some(format!("update check task: {join}")),
        }
    }

    let cached = state
        .update_cache
        .lock()
        .map(|cache| cache.clone())
        .unwrap_or(None);
    Ok(Json(
        crate::update::Status::build(true, current, cached.as_ref(), error).to_json(),
    ))
}

async fn download_backup(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let backup = localreel_media::export_backup(&state.db)?;
    let body = serde_json::to_vec_pretty(&backup)
        .map_err(|error| ApiError::internal(format!("failed to encode backup: {error}")))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"localreel-backup.json\"",
        )
        .body(Body::from(body))
        .map_err(|error| ApiError::internal(error.to_string()))
}

#[derive(Debug, Deserialize)]
struct BackupImportQuery {
    dry_run: Option<bool>,
}

async fn upload_backup(
    State(state): State<Arc<AppState>>,
    Query(query): Query<BackupImportQuery>,
    body: Bytes,
) -> Result<Json<BackupImportReport>, ApiError> {
    let backup = localreel_media::parse_backup(&body)?;
    Ok(Json(localreel_media::import_backup(
        &state.db,
        &backup,
        query.dry_run.unwrap_or(false),
    )?))
}

async fn enqueue_restore(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let backup = localreel_media::parse_backup(&body)?;
    let payload = serde_json::json!({ "backup": backup }).to_string();
    let job_id =
        state
            .jobs
            .enqueue("restore_backup", &payload, Some("restore_backup:manual"), 1)?;
    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

async fn upload_restore_file(
    State(state): State<Arc<AppState>>,
    body: Body,
) -> Result<Response, ApiError> {
    let upload_dir = state.config.data_dir.join("backup-uploads");
    tokio::fs::create_dir_all(&upload_dir)
        .await
        .map_err(|error| ApiError::internal(format!("cannot create upload dir: {error}")))?;
    let path = upload_dir.join(format!("{}.json", uuid::Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|error| ApiError::internal(format!("cannot create upload: {error}")))?;
    let mut stream = body.into_data_stream();
    let mut total = 0_u64;

    loop {
        let chunk = match futures_util::StreamExt::next(&mut stream).await {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(ApiError::bad_request(format!(
                    "upload read failed: {error}"
                )));
            }
            None => break,
        };
        total = total.saturating_add(chunk.len() as u64);
        if total > localreel_media::MAX_BACKUP_RESTORE_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            return Err(ApiError::payload_too_large(format!(
                "backup upload exceeds {} bytes",
                localreel_media::MAX_BACKUP_RESTORE_BYTES
            )));
        }
        if let Err(error) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            return Err(ApiError::internal(format!("cannot write upload: {error}")));
        }
    }
    if let Err(error) = file.flush().await {
        drop(file);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(ApiError::internal(format!("cannot flush upload: {error}")));
    }
    drop(file);
    if total == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(ApiError::bad_request("empty backup upload"));
    }

    let payload = serde_json::json!({ "path": path.to_string_lossy() }).to_string();
    match state.jobs.enqueue(
        "restore_backup_file",
        &payload,
        Some("restore_backup:manual"),
        1,
    ) {
        Ok(job_id) => Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "job_id": job_id })),
        )
            .into_response()),
        Err(error) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(error.into())
        }
    }
}

async fn cache_stats(State(state): State<Arc<AppState>>) -> Result<Json<CacheStats>, ApiError> {
    Ok(Json(state.media.cache_stats()?))
}

#[derive(Debug, Deserialize)]
struct CacheGcRequest {
    max_bytes: Option<u64>,
    max_age_secs: Option<u64>,
}

async fn run_cache_gc(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CacheGcRequest>,
) -> Result<Json<CacheGcReport>, ApiError> {
    let max_bytes = payload
        .max_bytes
        .unwrap_or(state.config.media.cache_max_bytes);
    let max_age_secs = payload
        .max_age_secs
        .unwrap_or(state.config.media.cache_max_age.as_secs());
    let max_age_secs = i64::try_from(max_age_secs).unwrap_or(i64::MAX);
    let report = state.media.gc_transcode_cache(
        &state.config.media.transcode_dir,
        max_bytes,
        max_age_secs,
        3600,
    )?;
    Ok(Json(report))
}

async fn list_scan_runs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<JobListQuery>,
) -> Result<Json<Vec<ScanRun>>, ApiError> {
    Ok(Json(state.media.list_scan_runs(query.limit.unwrap_or(50))?))
}

async fn list_scan_issues(
    State(state): State<Arc<AppState>>,
    Query(query): Query<JobListQuery>,
) -> Result<Json<Vec<ScanIssue>>, ApiError> {
    Ok(Json(
        state.media.list_scan_issues(query.limit.unwrap_or(50))?,
    ))
}

async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let receiver = state.events.subscribe();
    let stream = BroadcastStream::new(receiver).filter_map(|message| match message {
        Ok(event) => SseEvent::default().json_data(&event).ok().map(Ok),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
struct ListSourcesQuery {
    include_archived: Option<bool>,
}

async fn list_sources(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListSourcesQuery>,
) -> Result<Json<Vec<Source>>, ApiError> {
    Ok(Json(state.media.list_sources_with_archived(
        query.include_archived.unwrap_or(false),
    )?))
}

#[derive(Debug, Deserialize)]
struct AddSourceRequest {
    uri: String,
    #[serde(default)]
    kind: Option<String>,
}

async fn add_source(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AddSourceRequest>,
) -> Result<Response, ApiError> {
    let source = match payload.kind.as_deref() {
        Some(kind) => state.media.add_source_with_kind(&payload.uri, kind)?,
        None => state.media.add_source(&payload.uri)?,
    };
    // A new local folder starts being watched immediately; a plugin kind is
    // reported as skipped rather than retried.
    state.refresh_watchers();
    Ok((StatusCode::CREATED, Json(source)).into_response())
}

#[derive(Debug, Deserialize)]
struct PatchSourceRequest {
    enabled: Option<bool>,
    archived: Option<bool>,
}

async fn patch_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(payload): Json<PatchSourceRequest>,
) -> Result<Json<Source>, ApiError> {
    let source = if let Some(archived) = payload.archived {
        state.media.set_source_archived(id, archived)?
    } else if let Some(enabled) = payload.enabled {
        state.media.set_source_enabled(id, enabled)?
    } else {
        return Err(ApiError::bad_request("provide enabled or archived"));
    };
    state.refresh_watchers();
    Ok(Json(source))
}

#[derive(Debug, Deserialize)]
struct DeleteSourceQuery {
    delete_media: Option<bool>,
    dry_run: Option<bool>,
}

async fn delete_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(query): Query<DeleteSourceQuery>,
) -> Result<Json<DeleteSourceReport>, ApiError> {
    let report = state.media.delete_source(
        id,
        query.delete_media.unwrap_or(false),
        query.dry_run.unwrap_or(false),
        &state.config.media.thumbnail_dir,
        &state.config.media.transcode_dir,
    )?;
    if !report.dry_run {
        state.refresh_watchers();
    }
    Ok(Json(report))
}

#[derive(Debug, Deserialize)]
struct RemapSourceRequest {
    uri: String,
    #[serde(default)]
    dry_run: bool,
}

async fn remap_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(payload): Json<RemapSourceRequest>,
) -> Result<Json<RemapReport>, ApiError> {
    let report = state
        .media
        .remap_source(id, &payload.uri, payload.dry_run)?;
    if !report.dry_run {
        // A remap moves the root; the watcher has to follow it.
        state.refresh_watchers();
    }
    Ok(Json(report))
}

async fn scan_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let source = state
        .media
        .get_source(id)?
        .ok_or_else(|| ApiError::not_found(format!("source {id} not found")))?;
    if source.archived_at.is_some() {
        return Err(ApiError::bad_request(format!("source {id} is archived")));
    }
    if !source.enabled {
        return Err(ApiError::bad_request(format!("source {id} is disabled")));
    }
    let payload = serde_json::json!({ "source_id": id }).to_string();
    let job_id = state
        .jobs
        .enqueue("scan_source", &payload, Some(&format!("scan:{id}")), 1)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    source_id: Option<i64>,
    q: Option<String>,
    filter: Option<String>,
    include_archived: Option<bool>,
}

#[derive(Debug, Serialize)]
struct MediaPage {
    items: Vec<Media>,
    total: i64,
    next_cursor: Option<i64>,
}

#[derive(Debug, Serialize)]
struct FeedPage {
    items: Vec<FeedItem>,
    total: i64,
}

/// Ranked feed for the scroll view: continue-watching first, then unwatched
/// episodes in series order, then already-watched items.
async fn feed(
    State(state): State<Arc<AppState>>,
    Extension(current): Extension<CurrentUser>,
    Query(query): Query<ListQuery>,
) -> Result<Json<FeedPage>, ApiError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 200);
    let (items, total) = state.media.feed(current.user.id, limit)?;
    Ok(Json(FeedPage { items, total }))
}

async fn list_media(
    State(state): State<Arc<AppState>>,
    Extension(current): Extension<CurrentUser>,
    Query(query): Query<ListQuery>,
) -> Result<Json<MediaPage>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let filter = MediaFilter::parse(query.filter.as_deref().unwrap_or("all"))
        .ok_or_else(|| ApiError::bad_request("filter must be all, favorites or continue"))?;
    let (items, total) = state
        .media
        .list_media_page(localreel_media::MediaPageQuery {
            user_id: current.user.id,
            limit,
            offset,
            source_id: query.source_id,
            query: query.q.as_deref(),
            filter,
            include_archived_sources: query.include_archived.unwrap_or(false),
        })?;
    let next_cursor = if offset + (items.len() as i64) < total {
        Some(offset + items.len() as i64)
    } else {
        None
    };
    Ok(Json(MediaPage {
        items,
        total,
        next_cursor,
    }))
}

async fn search_media(
    State(state): State<Arc<AppState>>,
    Extension(current): Extension<CurrentUser>,
    Query(query): Query<ListQuery>,
) -> Result<Json<MediaPage>, ApiError> {
    list_media(State(state), Extension(current), Query(query)).await
}

/// One media item plus whatever external metadata matched it.
///
/// `metadata` and `poster_url` are omitted when no provider matched, so the
/// shape a client sees without metadata is exactly the old one.
#[derive(Debug, Serialize)]
struct MediaDetail {
    #[serde(flatten)]
    media: Media,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<localreel_media::MediaMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    poster_url: Option<String>,
}

async fn get_media(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<MediaDetail>, ApiError> {
    let media = state
        .media
        .get_media(id)?
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))?;
    let metadata = state.media.get_metadata(id)?;
    let poster_url = metadata
        .as_ref()
        .and_then(|metadata| metadata.poster_path.as_ref())
        .map(|path| format!("/posters/{path}"));
    Ok(Json(MediaDetail {
        media,
        metadata,
        poster_url,
    }))
}

/// What the optional metadata provider is doing.
async fn metadata_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "configured": state.config.metadata.provider.as_str(),
        "provider": state.metadata_provider,
        "language": state.config.metadata.language,
    }))
}

#[derive(Debug, Deserialize)]
struct MetadataMatchRequest {
    #[serde(default)]
    source_id: Option<i64>,
}

/// Queue a metadata match for one source, or for the whole library.
///
/// Only stale items are queued, so pressing the button twice does not re-query
/// the provider for everything.
async fn match_metadata(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<MetadataMatchRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.metadata_provider.is_none() {
        return Err(ApiError::bad_request(
            "no metadata provider is configured (set [metadata] provider and api_key)",
        ));
    }
    let ids = state.media.media_ids(payload.source_id)?;
    let mut queued = 0usize;
    let mut skipped = 0usize;
    for id in ids {
        if !state.media.metadata_is_stale(id, SEVEN_DAYS)? {
            skipped += 1;
            continue;
        }
        let payload = serde_json::json!({ "media_id": id }).to_string();
        match state.jobs.enqueue(
            "metadata_match",
            &payload,
            Some(&format!("metadata:{id}")),
            1,
        ) {
            Ok(_) => queued += 1,
            Err(error) => tracing::warn!(media_id = id, "failed to queue metadata match: {error}"),
        }
    }
    Ok(Json(serde_json::json!({
        "queued": queued,
        "fresh": skipped,
    })))
}

/// A matched item is re-queried at most this often.
const SEVEN_DAYS: i64 = 7 * 24 * 60 * 60;

/// Tracks of one media item.
///
/// A missing media is a 404 rather than an empty list: "this file has no other
/// audio" and "this id does not exist" are different answers.
async fn list_tracks(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<localreel_media::Track>>, ApiError> {
    state
        .media
        .get_media(id)?
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))?;
    Ok(Json(state.media.list_tracks(id)?))
}

/// The WebVTT track that turns the sprite sheet into progress-bar previews.
///
/// Generated on demand from the stored sprite and the media duration, so a
/// change in the sampling interval needs no cache invalidation.
async fn preview_vtt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let media = state
        .media
        .get_media(id)?
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))?;
    let missing = || ApiError::not_found(format!("media {id} has no preview"));
    let duration = media
        .duration
        .filter(|value| *value > 0.0)
        .ok_or_else(missing)?;
    let variant = state
        .media
        .find_variant(id, "sprite", "default")?
        .ok_or_else(missing)?;
    if !state.config.media.preview_dir.join(&variant.path).is_file() {
        return Err(missing());
    }

    let interval = localreel_kernel::media_ops::preview_interval(Some(duration));
    let tile = localreel_media::tile_size(media.width, media.height);
    let cues = localreel_media::preview_cues(duration, interval, tile);
    let body = localreel_media::preview_vtt(&format!("/previews/{}", variant.path), &cues);
    Ok((
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response())
}

async fn get_state(
    State(state): State<Arc<AppState>>,
    Extension(current): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Result<Json<MediaState>, ApiError> {
    Ok(Json(state.media.get_state(current.user.id, id)?))
}

#[derive(Debug, Deserialize)]
struct StatePatch {
    liked: Option<bool>,
    favorite: Option<bool>,
    position: Option<f64>,
    duration: Option<f64>,
}

async fn put_state(
    State(state): State<Arc<AppState>>,
    Extension(current): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(patch): Json<StatePatch>,
) -> Result<Json<MediaState>, ApiError> {
    // The service merges the patch inside one write transaction: progress saves
    // and like/favorite toggles arrive concurrently from the player.
    let saved = state.media.patch_state(
        current.user.id,
        id,
        patch.liked,
        patch.favorite,
        patch.position,
        patch.duration,
    )?;
    Ok(Json(saved))
}

#[derive(Debug, Deserialize)]
struct JobListQuery {
    limit: Option<i64>,
}

async fn list_jobs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<JobListQuery>,
) -> Result<Json<Vec<JobRecord>>, ApiError> {
    Ok(Json(
        state
            .jobs
            .list_recent(query.limit.unwrap_or(50).clamp(1, 200))?,
    ))
}

async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<JobRecord>, ApiError> {
    state
        .jobs
        .get(&id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))
}

async fn cancel_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.jobs.cancel(&id)? {
        return Err(ApiError::not_found(format!(
            "job {id} not found or not cancelable"
        )));
    }
    Ok(Json(serde_json::json!({ "canceled": true })))
}

#[derive(Debug, Deserialize)]
struct PlaybackQuery {
    format: Option<String>,
    /// Requested audio track id. Choosing anything but the default track forces
    /// a remux/transcode, because a browser cannot switch embedded tracks.
    audio: Option<i64>,
}

#[derive(Debug, Serialize)]
struct PlaybackResponse {
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Wait a moment for the probe job a scan queued.
///
/// The scan inserts the row first and fills the codecs shortly after. A play
/// request landing in that window used to choose the "unknown codec" preset - a
/// full re-encode of a file that only needs a remux - and two requests on either
/// side of the probe picked two different cache keys, so both jobs ran. Waiting
/// a few hundred milliseconds is much cheaper than either.
async fn settled_media(state: &AppState, media: Media) -> Result<Media, ApiError> {
    // Nothing to wait for when there is no ffprobe: the fields will never fill.
    if media.duration.is_some()
        || media.video_codec.is_some()
        || state.ffmpeg_capability == localreel_kernel::media_ops::FfmpegCapability::Missing
    {
        return Ok(media);
    }
    let mut latest = media;
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        match state.media.get_media(latest.id)? {
            Some(next) => {
                latest = next;
                if latest.duration.is_some() || latest.video_codec.is_some() {
                    break;
                }
            }
            None => break,
        }
    }
    Ok(latest)
}

/// Explain a previous failure instead of queueing the same work again.
///
/// A finished job releases its dedupe key, so `enqueue` would happily create a
/// fresh one on every poll; the client polls while the mode is `preparing`, so a
/// file that cannot be transcoded would spin forever. The failure is only
/// reported while it is newer than the media row, so a rescan (which bumps
/// `updated_at` after the file changed) clears it.
fn failed_attempt(
    state: &AppState,
    kind: &str,
    dedupe_key: &str,
    media: &Media,
) -> Result<Option<String>, ApiError> {
    let Some(job) = state.jobs.latest_for_dedupe(kind, dedupe_key)? else {
        return Ok(None);
    };
    if job.status != "failed" || job.finished_at.unwrap_or(0) < media.updated_at {
        return Ok(None);
    }
    Ok(Some(job.error.unwrap_or_else(|| {
        "this file could not be prepared".to_string()
    })))
}

async fn prepare_playback(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(query): Query<PlaybackQuery>,
) -> Result<Response, ApiError> {
    let media = state
        .media
        .get_media(id)?
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))?;
    // Any playback request keeps this media's session alive; the reaper cancels
    // work nobody has asked about for `idle_session_secs`.
    state.transcodes.touch(media.id);
    let media = settled_media(&state, media).await?;
    if media.missing_at.is_some() {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(PlaybackResponse {
                mode: "failed",
                url: None,
                job_id: None,
                message: Some("media file is missing".into()),
            }),
        )
            .into_response());
    }

    if query.format.as_deref() == Some("hls") {
        return prepare_hls_playback(state, &media);
    }

    // Resolve the requested audio track before deciding how to play: choosing a
    // non-default track rules out direct play, because the <video> element
    // cannot switch embedded tracks.
    let audio_track = match query.audio {
        Some(track_id) => {
            let track = state.media.get_track(media.id, track_id)?.ok_or_else(|| {
                ApiError::not_found(format!("track {track_id} of media {id} not found"))
            })?;
            if track.kind != "audio" {
                return Err(ApiError::bad_request("only audio tracks can be selected"));
            }
            Some(track)
        }
        None => None,
    };
    let switching_audio = audio_track.as_ref().is_some_and(|track| !track.is_default);
    let audio_stream = audio_track
        .as_ref()
        .and_then(|track| track.stream_index)
        .and_then(|index| usize::try_from(index).ok());

    if is_hls(&media) && !switching_audio {
        return Ok(Json(PlaybackResponse {
            mode: "direct",
            url: Some(media.uri),
            job_id: None,
            message: None,
        })
        .into_response());
    }

    if direct_play_supported(&media) && !switching_audio {
        return Ok(Json(PlaybackResponse {
            mode: "direct",
            url: Some(format!("/api/v1/media/{}/stream", media.id)),
            job_id: None,
            message: None,
        })
        .into_response());
    }

    let preset_name = preferred_preset(&media);
    let variant_key = localreel_media::transcode_variant_key(preset_name, audio_stream);

    if let Some(variant) = state
        .media
        .find_variant(media.id, "transcode", &variant_key)?
    {
        let path = state.config.media.transcode_dir.join(&variant.path);
        if path.is_file() {
            if let Err(error) = state.media.touch_variant(media.id, &variant_key) {
                tracing::warn!(
                    media_id = media.id,
                    "failed to touch transcode variant: {error}"
                );
            }
            return Ok(Json(PlaybackResponse {
                mode: "ready",
                url: Some(format!("/transcodes/{}", variant.path)),
                job_id: None,
                message: None,
            })
            .into_response());
        }
    }

    let payload = serde_json::json!({
        "media_id": media.id,
        "preset": preset_name,
        "audio_stream": audio_stream,
    })
    .to_string();
    let transcode_key = format!("transcode:{}:{variant_key}", media.id);
    if let Some(reason) = failed_attempt(&state, "transcode", &transcode_key, &media)? {
        return Ok(Json(PlaybackResponse {
            mode: "failed",
            url: None,
            job_id: None,
            message: Some(reason),
        })
        .into_response());
    }
    // Starting an encode on a machine that is out of memory only makes it
    // slower: say so instead of piling another ffmpeg onto the box.
    if localreel_media::memory_pressure() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "not enough free memory to start a transcode right now",
        ));
    }
    let job_id = state
        .jobs
        .enqueue("transcode", &payload, Some(&transcode_key), 2)?;
    state.transcodes.attach_job(media.id, "transcode", &job_id);
    Ok((
        StatusCode::ACCEPTED,
        Json(PlaybackResponse {
            mode: "preparing",
            url: None,
            job_id: Some(job_id),
            message: Some("transcode started".into()),
        }),
    )
        .into_response())
}

fn prepare_hls_playback(
    state: Arc<AppState>,
    media: &localreel_media::Media,
) -> Result<Response, ApiError> {
    if let Some(variant) = state.media.find_variant(media.id, "hls", "default")? {
        let path = state.config.media.hls_dir.join(&variant.path);
        if path.is_file() {
            return Ok(Json(PlaybackResponse {
                mode: "ready",
                url: Some(format!("/hls/{}", variant.path)),
                job_id: None,
                message: None,
            })
            .into_response());
        }
    }

    let hls_key = format!("hls:{}", media.id);
    if let Some(reason) = failed_attempt(&state, "hls_package", &hls_key, media)? {
        return Ok(Json(PlaybackResponse {
            mode: "failed",
            url: None,
            job_id: None,
            message: Some(reason),
        })
        .into_response());
    }
    let payload = serde_json::json!({ "media_id": media.id }).to_string();
    if localreel_media::memory_pressure() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "not enough free memory to package this stream right now",
        ));
    }
    let job_id = state
        .jobs
        .enqueue("hls_package", &payload, Some(&hls_key), 2)?;
    state
        .transcodes
        .attach_job(media.id, "hls_package", &job_id);
    Ok((
        StatusCode::ACCEPTED,
        Json(PlaybackResponse {
            mode: "preparing",
            url: None,
            job_id: Some(job_id),
            message: Some("hls packaging started".into()),
        }),
    )
        .into_response())
}

async fn serve_hls(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Result<Response, ApiError> {
    use std::path::{Component, Path as FsPath};

    // Only plain relative components; no `..`, no drive prefixes, no roots.
    let relative = FsPath::new(&path);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ApiError::not_found("hls file not found"));
    }
    let file = state.config.media.hls_dir.join(relative);
    // Segments are served while the player plays; keep the session alive so the
    // reaper does not cancel a package someone is actively watching.
    if let Some(uid) = path.split('/').next() {
        if let Ok(Some(media)) = state.media.get_media_by_uid(uid) {
            state.transcodes.touch(media.id);
        }
    }
    let bytes = std::fs::read(&file).map_err(|_| ApiError::not_found("hls file not found"))?;
    let mime = match file.extension().and_then(|extension| extension.to_str()) {
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("ts") => "video/mp2t",
        _ => "application/octet-stream",
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response())
}

async fn stream_media(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    serve_media(state, id, headers, false).await
}

async fn head_media(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    serve_media(state, id, headers, true).await
}

struct ResolvedMedia {
    uri: MediaUri,
    storage: Arc<dyn Storage>,
    size: u64,
    modified: i64,
    etag: String,
    mime: String,
}

async fn resolve_media(state: &AppState, id: i64) -> Result<ResolvedMedia, ApiError> {
    let media = state
        .media
        .get_media(id)?
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))?;
    let uri = MediaUri::parse(&media.uri)?;
    let source = state
        .media
        .get_source(media.source_id)?
        .ok_or_else(|| ApiError::not_found("media source is missing"))?;
    let storage: Arc<dyn Storage> = state.media.storage_for(&source).map_err(|error| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            format!("no storage provider for this source: {error}"),
        )
    })?;
    let stat = storage.stat(&uri).map_err(|error| match error {
        localreel_kernel::error::KernelError::NotFound(_) => {
            ApiError::not_found("media file is missing")
        }
        _ => ApiError::internal(format!("failed to stat media: {error}")),
    })?;
    if stat.is_dir {
        return Err(ApiError::bad_request("media path is not a file"));
    }
    let modified = stat.modified.unwrap_or(0);
    let etag = format!("W/\"{:x}-{:x}\"", stat.size, modified);
    // The stored file name works for local paths and remote URIs alike.
    let mime = mime_guess::from_path(&media.filename)
        .first_or_octet_stream()
        .to_string();
    Ok(ResolvedMedia {
        uri,
        storage,
        size: stat.size,
        modified,
        etag,
        mime,
    })
}

async fn serve_media(
    state: Arc<AppState>,
    id: i64,
    headers: HeaderMap,
    head_only: bool,
) -> Result<Response, ApiError> {
    let media = resolve_media(&state, id).await?;

    // Streaming is the strongest "still watching" signal there is.
    state.transcodes.touch(id);

    let last_modified = http_date(media.modified);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    let if_modified_since = headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok());
    let not_modified = match (if_none_match, if_modified_since) {
        (Some(value), _) => if_none_match_matches(value, &media.etag),
        (None, Some(value)) => if_modified_since_not_modified(value, media.modified),
        (None, None) => false,
    };
    if not_modified {
        let mut builder = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &media.etag);
        if let Some(date) = &last_modified {
            builder = builder.header(header::LAST_MODIFIED, date);
        }
        return builder
            .body(Body::empty())
            .map_err(|error| ApiError::internal(error.to_string()));
    }

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let if_range = headers
        .get(header::IF_RANGE)
        .and_then(|value| value.to_str().ok());
    let honor_range = range_header.is_some()
        && if_range.is_none_or(|value| if_range_matches(value, &media.etag, media.modified));
    let range = if honor_range {
        range_header.and_then(|value| parse_range(value, media.size))
    } else {
        None
    };
    if honor_range && range.is_none() {
        let mut builder = Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", media.size))
            .header(header::ETAG, &media.etag)
            .header(header::ACCEPT_RANGES, "bytes");
        if let Some(date) = &last_modified {
            builder = builder.header(header::LAST_MODIFIED, date);
        }
        return builder
            .body(Body::empty())
            .map_err(|error| ApiError::internal(error.to_string()));
    }

    if media.size == 0 {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, &media.mime)
            .header(header::CONTENT_LENGTH, "0")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ETAG, &media.etag);
        if let Some(date) = &last_modified {
            builder = builder.header(header::LAST_MODIFIED, date);
        }
        return builder
            .body(Body::empty())
            .map_err(|error| ApiError::internal(error.to_string()));
    }

    let (start, end, status) = match range {
        Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
        None => (0, media.size - 1, StatusCode::OK),
    };
    let length = end - start + 1;

    let body = if head_only {
        Body::empty()
    } else {
        let storage = media.storage.clone();
        let uri = media.uri.clone();
        let mut reader =
            tokio::task::spawn_blocking(move || storage.open_range(&uri, start, length))
                .await
                .map_err(|error| ApiError::internal(format!("reader task failed: {error}")))?
                .map_err(|error| match error {
                    localreel_kernel::error::KernelError::NotFound(_) => {
                        ApiError::not_found("media file is missing")
                    }
                    _ => ApiError::internal(format!("failed to open media: {error}")),
                })?;
        let (sender, receiver) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match Read::read(&mut *reader, &mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if sender
                            .blocking_send(Ok(Bytes::copy_from_slice(&buffer[..read])))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.blocking_send(Err(error));
                        break;
                    }
                }
            }
        });
        let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        });
        Body::from_stream(stream)
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, &media.mime)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &media.etag)
        .header(header::CACHE_CONTROL, "private, max-age=3600");
    if let Some(date) = &last_modified {
        builder = builder.header(header::LAST_MODIFIED, date);
    }
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", media.size),
        );
    }
    builder
        .body(body)
        .map_err(|error| ApiError::internal(error.to_string()))
}

fn if_none_match_matches(value: &str, etag: &str) -> bool {
    let target = etag.trim_start_matches("W/");
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.trim_start_matches("W/") == target
    })
}

fn http_date(seconds: i64) -> Option<String> {
    if seconds <= 0 {
        return None;
    }
    let time = UNIX_EPOCH + Duration::from_secs(seconds as u64);
    Some(httpdate::fmt_http_date(time))
}

fn parse_http_date_secs(value: &str) -> Option<i64> {
    httpdate::parse_http_date(value.trim())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

fn if_modified_since_not_modified(value: &str, modified: i64) -> bool {
    modified > 0 && parse_http_date_secs(value).is_some_and(|date| modified <= date)
}

/// `If-Range` accepts either an entity tag or an HTTP date. A matching weak tag
/// is treated like a matching strong tag: the host only emits weak tags, and a
/// strict reading would make resumption impossible.
fn if_range_matches(value: &str, etag: &str, modified: i64) -> bool {
    let value = value.trim();
    if value.starts_with("W/") || value.starts_with('"') {
        return value == etag;
    }
    parse_http_date_secs(value).is_some_and(|date| modified > 0 && modified <= date)
}

fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let bytes = value.strip_prefix("bytes=")?;
    if bytes.contains(',') {
        return None;
    }
    let (start, end) = bytes.split_once('-')?;
    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let length = suffix.min(size).min(MAX_RANGE_CHUNK);
        return Some((size - length, size - 1));
    }

    let start: u64 = start.parse().ok()?;
    if start >= size {
        return None;
    }
    let requested_end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    if start > requested_end {
        return None;
    }
    let end = requested_end.min(start.saturating_add(MAX_RANGE_CHUNK - 1));
    Some((start, end))
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>LocalReel</title>
  <style>
    :root { color-scheme: light; }
    body { margin: 0; min-height: 100vh; display: grid; place-items: center;
           background: #f3f0e8; color: #1f1d18;
           font: 16px/1.5 "Segoe UI", system-ui, sans-serif; }
    main { max-width: 34rem; padding: 2rem; }
    h1 { font-family: Georgia, "Times New Roman", serif; font-size: 2.5rem; margin: 0 0 .5rem; }
    p { color: #686159; }
    code { background: #e9e4d9; padding: .15rem .35rem; border-radius: .35rem; }
    a { color: #9d4f32; }
  </style>
</head>
<body>
  <main>
    <h1>LocalReel</h1>
    <p>Local-first media server. The API is at <code>/api/v1</code>.</p>
    <p><a href="/api/v1/health">Health</a> 路 <a href="/api/v1/sources">Sources</a> 路 <a href="/api/v1/media">Media</a></p>
    <p>The product UI is built separately in <code>localreel-web</code> and mounted here in a later phase.</p>
  </main>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use localreel_kernel::config::Config;
    use localreel_kernel::db::Database;
    use localreel_kernel::events::EventBus;
    use localreel_kernel::jobs::JobQueue;
    use localreel_kernel::migrations;
    use localreel_media::MediaService;
    use tower::ServiceExt;

    fn state(dir: &std::path::Path) -> Arc<AppState> {
        let db = Database::open(dir.join("api.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        migrations::apply(&db, "media", localreel_media::migrations::list()).unwrap();
        migrations::apply(&db, "plugin", localreel_plugin::audit::migrations()).unwrap();
        let events = EventBus::new(16);
        let jobs = JobQueue::new(db.clone(), events.clone());
        // Keep every derived directory inside the temp dir: a test must never
        // touch the operator's real cache.
        let mut config = Config::default();
        config.set_data_dir(dir.to_path_buf());
        let media = MediaService::new(db.clone());
        let accounts = localreel_media::AccountService::new(db.clone());
        let transcodes = Arc::new(localreel_media::TranscodeGate::new(
            2,
            std::time::Duration::from_secs(900),
        ));
        let plugins = Arc::new(localreel_plugin::PluginHost::new(db.clone()));
        AppState::new(crate::state::AppStateParts {
            config,
            db,
            events,
            jobs,
            media,
            accounts,
            transcodes,
            plugins,
            config_source: None,
            metadata_provider: None,
            ffmpeg_capability: localreel_kernel::media_ops::FfmpegCapability::Missing,
            hwaccel: localreel_kernel::media_ops::HwAccel::None,
        })
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("host", "127.0.0.1:3001")
            .body(Body::empty())
            .unwrap()
    }

    /// A state with a configured owner and one live session.
    fn signed_in(dir: &std::path::Path) -> (Arc<AppState>, String, String) {
        let state = state(dir);
        let (session, csrf) = sign_in(&state);
        (state, session, csrf)
    }

    fn sign_in(state: &Arc<AppState>) -> (String, String) {
        let owner = state
            .accounts
            .setup_owner("owner", "correct horse")
            .unwrap();
        let session = state.accounts.create_session(owner.id).unwrap();
        (session.id, session.csrf)
    }

    fn request_as(uri: &str, session: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("host", "127.0.0.1:3001")
            .header("cookie", format!("localreel_session={session}"))
            .body(Body::empty())
            .unwrap()
    }

    fn request_json(
        method: &str,
        uri: &str,
        session: &str,
        csrf: &str,
        body: String,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:3001")
            .header("cookie", format!("localreel_session={session}"))
            .header("x-localreel-csrf", csrf)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn security_headers_are_set() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        let response = app.oneshot(request("/api/v1/health")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
        assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
        assert!(headers
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("default-src 'self'"));
        assert!(headers.get("permissions-policy").is_some());
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        let response = app.oneshot(request("/api/v1/health")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("\"status\":\"ok\""));
        assert!(text.contains("\"database\":true"));
    }

    /// A direct-playable media row with a default and a second audio track.
    ///
    /// Raw SQL because the media service has no "insert media" entry point (the
    /// scanner owns that), and this test is about the HTTP contract.
    fn seed_playable(state: &AppState, dir: &std::path::Path) -> (i64, i64, i64) {
        let library = dir.join("library");
        std::fs::create_dir_all(&library).unwrap();
        let source = state.media.add_source(library.to_str().unwrap()).unwrap();
        let now = localreel_kernel::time::now_secs();
        let uri = library.join("movie.mp4").to_string_lossy().to_string();
        state
            .db
            .with_write(|connection| {
                connection.execute(
                    "INSERT INTO media (uid, source_id, uri, canonical_path, filename,
                         video_codec, audio_codec, file_size, modified_at, created_at, updated_at)
                     VALUES ('playable', ?1, ?2, ?2, 'movie.mp4', 'h264', 'aac', 0, 0, ?3, ?3)",
                    (source.id, uri, now),
                )?;
                let media_id = connection.last_insert_rowid();
                let track = |index: i64, language: &str, is_default: i64| {
                    connection.execute(
                        "INSERT INTO media_tracks (media_id, kind, source, stream_index,
                             format, language, is_default, is_forced, created_at)
                         VALUES (?1, 'audio', 'embedded', ?2, 'aac', ?3, ?4, 0, ?5)",
                        (media_id, index, language, is_default, now),
                    )?;
                    Ok::<i64, localreel_kernel::error::KernelError>(connection.last_insert_rowid())
                };
                let first = track(1, "ja", 1)?;
                let second = track(2, "en", 0)?;
                Ok((media_id, first, second))
            })
            .unwrap()
    }

    #[tokio::test]
    async fn tracks_list_the_audio_menu() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let (media_id, _, _) = seed_playable(&state, dir.path());
        let app = router(state);

        let response = app
            .oneshot(request_as(
                &format!("/api/v1/media/{media_id}/tracks"),
                &session,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let tracks: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(tracks.as_array().unwrap().len(), 2);
        assert_eq!(tracks[0]["language"], "ja");
        assert_eq!(tracks[0]["is_default"], true);
        assert_eq!(tracks[0]["kind"], "audio");
        assert_eq!(tracks[1]["stream_index"], 2);
        assert_eq!(tracks[1]["is_default"], false);
    }

    #[tokio::test]
    async fn tracks_need_a_session_and_an_existing_media() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let app = router(state);

        // No session: the tracker is behind the ordinary sign-in wall.
        let response = app
            .clone()
            .oneshot(request("/api/v1/media/1/tracks"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(request_as("/api/v1/media/999/tracks", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn choosing_the_default_track_keeps_direct_play() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let (media_id, default_track, _other) = seed_playable(&state, dir.path());
        let app = router(state);

        let response = app
            .oneshot(request_json(
                "POST",
                &format!("/api/v1/media/{media_id}/playback?audio={default_track}"),
                &session,
                &csrf,
                "{}".to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let playback: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(playback["mode"], "direct");
    }

    #[tokio::test]
    async fn choosing_another_track_forces_a_transcode() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let (media_id, _default_track, other) = seed_playable(&state, dir.path());
        let jobs = state.jobs.clone();
        let app = router(state);

        let response = app
            .oneshot(request_json(
                "POST",
                &format!("/api/v1/media/{media_id}/playback?audio={other}"),
                &session,
                &csrf,
                "{}".to_string(),
            ))
            .await
            .unwrap();
        // A browser cannot switch embedded audio tracks, so the answer is a job.
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let playback: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(playback["mode"], "preparing");

        let jobs = jobs.list_recent(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "transcode");
        let payload: serde_json::Value = serde_json::from_str(&jobs[0].payload).unwrap();
        assert_eq!(payload["audio_stream"], 2);
        // The audio choice is part of the cache key, so languages do not collide.
        assert!(jobs[0]
            .dedupe_key
            .as_deref()
            .is_some_and(|key| key.ends_with("_a2")));
    }

    #[tokio::test]
    async fn an_unknown_track_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let (media_id, _, _) = seed_playable(&state, dir.path());
        let app = router(state);

        let response = app
            .oneshot(request_json(
                "POST",
                &format!("/api/v1/media/{media_id}/playback?audio=999"),
                &session,
                &csrf,
                "{}".to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn preview_vtt_describes_the_sprite_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let library = dir.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let source = state.media.add_source(library.to_str().unwrap()).unwrap();
        let now = localreel_kernel::time::now_secs();
        let media_id = state
            .db
            .with_write(|connection| {
                connection.execute(
                    "INSERT INTO media (uid, source_id, uri, canonical_path, filename,
                         duration, width, height, file_size, modified_at, created_at, updated_at)
                     VALUES ('previewme', ?1, ?2, ?2, 'long.mp4', 600.0, 1920, 1080, 0, 0, ?3, ?3)",
                    (
                        source.id,
                        library.join("long.mp4").to_string_lossy().to_string(),
                        now,
                    ),
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();

        // No sprite yet: the client must get a 404, not an empty document.
        let app = router(state.clone());
        let response = app
            .oneshot(request_as(
                &format!("/api/v1/media/{media_id}/preview.vtt"),
                &session,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let filename = "previewme.jpg";
        std::fs::create_dir_all(&state.config.media.preview_dir).unwrap();
        std::fs::write(state.config.media.preview_dir.join(filename), b"jpeg").unwrap();
        state
            .media
            .record_variant(media_id, "sprite", "default", filename, 4)
            .unwrap();
        state
            .db
            .with_write(|connection| {
                connection.execute(
                    "UPDATE media SET thumbnail_path = 'previewme.jpg' WHERE id = ?1",
                    [media_id],
                )?;
                Ok(())
            })
            .unwrap();

        let app = router(state);
        let response = app
            .oneshot(request_as(
                &format!("/api/v1/media/{media_id}/preview.vtt"),
                &session,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/vtt"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.starts_with("WEBVTT\n\n"));
        assert!(text.contains("/previews/previewme.jpg#xywh=0,0,160,90"));
        // Ten minutes at six seconds per tile: one hundred cues.
        assert_eq!(text.matches("-->").count(), 100);
    }

    #[tokio::test]
    async fn the_plugin_proxy_requires_a_plugin_token() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));

        // No token at all: the request never reaches the proxy.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/plugin-proxy")
                    .header("host", "127.0.0.1:3001")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"url":"https://example.com"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A token no running plugin owns is a permission error, not a fetch.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/plugin-proxy")
                    .header("host", "127.0.0.1:3001")
                    .header("x-localreel-plugin", "not-a-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"url":"https://example.com"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_admin_plugin_list_is_empty_without_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let app = router(state);

        for (uri, key) in [
            ("/api/v1/plugins", "plugins"),
            ("/api/v1/plugins/audit", "entries"),
        ] {
            let response = app
                .clone()
                .oneshot(request_as(uri, &session))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value[key], serde_json::json!([]), "{uri}");
        }
    }

    #[tokio::test]
    async fn a_failed_plugin_is_reported_instead_of_vanishing() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        // The loader records failures instead of returning an error; the API
        // must surface them with the reason the operator needs.
        state.plugins.record_failed(
            &std::path::PathBuf::from("plugins/broken.lrplug"),
            None,
            "plugin did not print a ready line in time",
        );
        let app = router(state);

        let response = app
            .oneshot(request_as("/api/v1/plugins", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let plugin = &value["plugins"][0];
        assert_eq!(plugin["id"], "broken");
        assert_eq!(plugin["state"], "failed");
        assert_eq!(plugin["running"], false);
        assert_eq!(plugin["error"], "plugin did not print a ready line in time");
    }

    #[tokio::test]
    async fn plugin_capabilities_are_disclosed_and_grantable() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let (session, csrf) = sign_in(&state);
        let manifest = localreel_plugin::Manifest::parse(
            br#"{
                "id": "probe",
                "name": "Capability probe",
                "version": "0.1.0",
                "api_version": "localreel.plugin/v1",
                "kind": "storage",
                "entry": { "program": "plugin-probe", "args": [] },
                "permissions": ["network"],
                "capabilities": [
                    { "name": "net.fetch", "scope": { "hosts": ["api.example.com"] } }
                ]
            }"#,
        )
        .unwrap();
        state.plugins.record_failed(
            &std::path::PathBuf::from("plugins/probe.lrplug"),
            Some(&manifest),
            "not started in this test",
        );
        let app = router(state);

        let response = app
            .clone()
            .oneshot(request_as("/api/v1/plugins/probe/capabilities", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["plugin_id"], "probe");
        assert!(value["requested"]
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["name"] == "net.fetch" && request["source"] == "manifest"));
        // The storage baseline still shows up as a legacy request.
        assert!(value["requested"]
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["name"] == "fs.read" && request["source"] == "legacy"));
        assert!(!value["catalog"].as_array().unwrap().is_empty());

        let grant = serde_json::json!({
            "capability": "net.fetch",
            "scope": { "hosts": ["api.example.com"] }
        });
        let response = app
            .clone()
            .oneshot(request_json(
                "POST",
                "/api/v1/plugins/probe/capabilities",
                &session,
                &csrf,
                grant.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["capability"], "net.fetch");
        assert_eq!(value["tier"], 2);
        assert_eq!(value["scope"]["hosts"][0], "api.example.com");

        let response = app
            .clone()
            .oneshot(request_json(
                "DELETE",
                "/api/v1/plugins/probe/capabilities/net.fetch",
                &session,
                &csrf,
                String::new(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["revoked"], true);

        // The grant row stays as history, but it is marked revoked.
        let response = app
            .oneshot(request_as("/api/v1/plugins/probe/capabilities", &session))
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value["granted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|grant| grant["capability"] == "net.fetch" && grant["revoked_at"].is_number()));
    }

    #[tokio::test]
    async fn controlling_an_unknown_plugin_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let app = router(state);

        let response = app
            .clone()
            .oneshot(request_json(
                "POST",
                "/api/v1/plugins/nope/restart",
                &session,
                &csrf,
                String::new(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = app
            .oneshot(request_as("/api/v1/plugins/nope/logs", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn media_detail_carries_its_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let (media_id, _, _) = seed_playable(&state, dir.path());
        let candidate = localreel_media::MetadataCandidate {
            provider_id: "603".into(),
            title: "The Matrix".into(),
            overview: Some("A hacker learns the truth.".into()),
            release_date: Some("1999-03-31".into()),
            vote_average: Some(8.2),
            poster_url: None,
            backdrop_url: None,
            confidence: 1.0,
        };
        state
            .media
            .upsert_metadata(media_id, "tmdb", &candidate, Some("603.jpg"))
            .unwrap();
        let app = router(state);

        let response = app
            .oneshot(request_as(&format!("/api/v1/media/{media_id}"), &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Flattened: the media fields stay at the top level.
        assert_eq!(detail["id"], media_id);
        assert_eq!(detail["metadata"]["title"], "The Matrix");
        assert_eq!(detail["metadata"]["provider"], "tmdb");
        assert_eq!(detail["poster_url"], "/posters/603.jpg");
    }

    #[tokio::test]
    async fn metadata_status_reports_that_nothing_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let app = router(state);

        let response = app
            .oneshot(request_as("/api/v1/metadata", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["configured"], "none");
        assert!(status["provider"].is_null(), "no provider was built");
    }

    #[tokio::test]
    async fn matching_without_a_provider_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let app = router(state);

        let response = app
            .oneshot(request_json(
                "POST",
                "/api/v1/metadata/match",
                &session,
                &csrf,
                "{}".to_string(),
            ))
            .await
            .unwrap();
        // A button that cannot do anything should say why, not queue nothing.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn status_hands_the_csrf_token_back_to_a_reloaded_page() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let app = router(state);

        let response = app
            .clone()
            .oneshot(request_as("/api/v1/auth/status", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["authenticated"], true);
        assert_eq!(
            status["csrf"].as_str(),
            Some(csrf.as_str()),
            "a reload keeps the cookie but needs the token back"
        );

        // Without a session there is no token to hand out.
        let anonymous = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/status")
                    .header("host", "127.0.0.1:3001")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(anonymous.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["authenticated"], false);
        assert!(status.get("csrf").is_none(), "no session, no token");
    }

    #[tokio::test]
    async fn a_write_without_the_csrf_header_is_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let app = router(state);

        // The session cookie alone must not authorize a write: that is the
        // whole point of the token.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/sources")
                    .header("host", "127.0.0.1:3001")
                    .header("content-type", "application/json")
                    .header("cookie", format!("localreel_session={session}"))
                    .body(Body::from(
                        serde_json::json!({ "uri": dir.path().to_string_lossy() }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_failed_transcode_is_reported_instead_of_requeued() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let (media_id, _, _) = seed_playable(&state, dir.path());
        // A file the browser cannot play directly, so the request reaches the
        // transcode path instead of answering "direct".
        state
            .db
            .with_write(|connection| {
                connection.execute(
                    "UPDATE media SET video_codec = 'hevc' WHERE id = ?1",
                    [media_id],
                )?;
                Ok(())
            })
            .unwrap();

        // Leave behind the row a worker would: one failed attempt for this variant.
        let media = state.media.get_media(media_id).unwrap().unwrap();
        let preset = preferred_preset(&media);
        let variant = localreel_media::transcode_variant_key(preset, None);
        let dedupe = format!("transcode:{media_id}:{variant}");
        let payload = serde_json::json!({
            "media_id": media_id,
            "preset": preset,
            "audio_stream": null,
        })
        .to_string();
        let job_id = state
            .jobs
            .enqueue("transcode", &payload, Some(&dedupe), 1)
            .unwrap();
        let claimed = state.jobs.claim("worker", 60).unwrap().unwrap();
        assert_eq!(claimed.id, job_id);
        state.jobs.fail(&job_id, "ffmpeg exploded").unwrap();

        let app = router(state);
        let response = app
            .oneshot(request_json(
                "POST",
                &format!("/api/v1/media/{media_id}/playback"),
                &session,
                &csrf,
                "{}".to_string(),
            ))
            .await
            .unwrap();
        // The player polls while "preparing"; without this it would spin forever
        // and queue a fresh job on every poll.
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let playback: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(playback["mode"], "failed");
        assert!(
            playback["message"]
                .as_str()
                .unwrap_or_default()
                .contains("ffmpeg exploded"),
            "the reason must reach the player: {playback:?}"
        );
    }

    /// The admin health endpoint drives the "watching / not watching" badge.
    #[tokio::test]
    async fn admin_health_reports_the_watchers() {
        let dir = tempfile::tempdir().unwrap();
        let library = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let app = router(state);

        let response = app
            .clone()
            .oneshot(request_as("/api/v1/admin/health", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["watchers"]["enabled"], true);
        assert_eq!(health["watchers"]["active"], 0);
        assert_eq!(health["watchers"]["skipped"], serde_json::json!([]));
        // The transcode budget is reported even when nothing is running.
        assert_eq!(health["transcodes"]["limit"], 2);
        assert_eq!(health["transcodes"]["running"], 0);
        assert_eq!(health["transcodes"]["queued"], 0);
        assert_eq!(health["transcodes"]["sessions"], 0);

        // Adding a local source subscribes to it immediately.
        let response = app
            .clone()
            .oneshot(request_json(
                "POST",
                "/api/v1/sources",
                &session,
                &csrf,
                serde_json::json!({ "uri": library.path() }).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = app
            .oneshot(request_as("/api/v1/admin/health", &session))
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["watchers"]["active"], 1);
    }

    #[tokio::test]
    async fn backup_download_and_upload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, csrf) = signed_in(dir.path());
        let app = router(state);
        let response = app
            .clone()
            .oneshot(request_as("/api/v1/backup", &session))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .is_some());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let backup: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(backup["version"], localreel_media::BACKUP_VERSION);

        let response = app
            .oneshot(request_json(
                "POST",
                "/api/v1/backup",
                &session,
                &csrf,
                String::from_utf8(body.to_vec()).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(report["states"], 0);
    }

    #[tokio::test]
    async fn unauthenticated_api_requests_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        for uri in [
            "/api/v1/media",
            "/api/v1/feed",
            "/api/v1/sources",
            "/api/v1/users",
            "/api/v1/jobs",
        ] {
            let response = app.clone().oneshot(request(uri)).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} must require a session"
            );
        }
        // The launcher probe and the sign-in endpoints stay public.
        let response = app
            .clone()
            .oneshot(request("/api/v1/health"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app.oneshot(request("/api/v1/auth/status")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn state_changing_requests_need_the_csrf_header() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session, _csrf) = signed_in(dir.path());
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/sources/1")
                    .header("host", "127.0.0.1:3001")
                    .header("cookie", format!("localreel_session={session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"archived":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn regular_accounts_cannot_reach_admin_routes() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _session, _csrf) = signed_in(dir.path());
        let guest = state
            .accounts
            .create_user("guest", "guest password", localreel_media::Role::User)
            .unwrap();
        let guest_session = state.accounts.create_session(guest.id).unwrap();
        let app = router(state);

        let response = app
            .clone()
            .oneshot(request_as("/api/v1/sources", &guest_session.id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .oneshot(request_as("/api/v1/media", &guest_session.id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn only_an_administrator_can_stop_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let (state, owner_session, owner_csrf) = signed_in(dir.path());
        let notify = state.shutdown.clone();
        let app = router(state.clone());

        let guest = state
            .accounts
            .create_user("guest", "guest password", localreel_media::Role::User)
            .unwrap();
        let guest_session = state.accounts.create_session(guest.id).unwrap();
        let response = app
            .clone()
            .oneshot(request_json(
                "POST",
                "/api/v1/admin/shutdown",
                &guest_session.id,
                &guest_session.csrf,
                "{}".into(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let signalled = notify.notified();
        let response = app
            .oneshot(request_json(
                "POST",
                "/api/v1/admin/shutdown",
                &owner_session,
                &owner_csrf,
                "{}".into(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(std::time::Duration::from_millis(500), signalled)
            .await
            .expect("the shutdown signal was not raised");
    }

    #[tokio::test]
    async fn per_user_state_is_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let (state, owner_session, owner_csrf) = signed_in(dir.path());
        let guest = state
            .accounts
            .create_user("guest", "guest password", localreel_media::Role::User)
            .unwrap();
        let guest_session = state.accounts.create_session(guest.id).unwrap();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        state
            .media
            .add_source(source_dir.to_str().unwrap())
            .unwrap();
        let media_id = state
            .db
            .with_write(|connection| {
                connection.execute(
                    "INSERT INTO media (uid, source_id, uri, canonical_path, filename, created_at, updated_at)
                     VALUES ('uid-1', 1, 'file:///videos/a.mp4', 'videos/a.mp4', 'a.mp4', 0, 0)",
                    [],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        let app = router(state);

        let response = app
            .clone()
            .oneshot(request_json(
                "PUT",
                &format!("/api/v1/media/{media_id}/state"),
                &owner_session,
                &owner_csrf,
                r#"{"favorite":true,"position":42.0,"duration":100.0}"#.into(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The guest sees their own (empty) state, not the owner's.
        let response = app
            .oneshot(request_as(
                &format!("/api/v1/media/{media_id}/state"),
                &guest_session.id,
            ))
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let state_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(state_json["favorite"], false);
        assert_eq!(state_json["position"], 0.0);
    }

    #[tokio::test]
    async fn admin_deep_link_serves_the_web_shell() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        let response = app.oneshot(request("/admin")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("LocalReel"));
    }

    #[tokio::test]
    async fn watch_deep_link_serves_the_web_shell() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        let response = app.oneshot(request("/watch")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("LocalReel"));
    }

    #[tokio::test]
    async fn packaged_build_serves_the_pwa_assets() {
        if !crate::web::is_embedded() {
            // CI builds the core without a web checkout, so there is nothing to
            // serve; the composition check happens in the product repository.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        let manifest = app
            .clone()
            .oneshot(request("/manifest.webmanifest"))
            .await
            .unwrap();
        assert_eq!(manifest.status(), StatusCode::OK);
        let body = axum::body::to_bytes(manifest.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("LocalReel"));

        let icon = app.oneshot(request("/icons/icon-192.png")).await.unwrap();
        assert_eq!(icon.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn archive_source_hides_it_and_blocks_scans() {
        let dir = tempfile::tempdir().unwrap();
        let (app_state, session, csrf) = signed_in(dir.path());
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = app_state
            .media
            .add_source(source_dir.to_str().unwrap())
            .unwrap();
        let app = router(app_state);

        let patch = |body: &str| {
            request_json(
                "PATCH",
                &format!("/api/v1/sources/{}", source.id),
                &session,
                &csrf,
                body.to_string(),
            )
        };

        let response = app
            .clone()
            .oneshot(patch(r#"{"archived":true}"#))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let source_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(source_json["archived_at"].is_number());

        let response = app
            .clone()
            .oneshot(request_as("/api/v1/sources", &session))
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let active: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(active.as_array().unwrap().len(), 0);

        let response = app
            .clone()
            .oneshot(request_as(
                "/api/v1/sources?include_archived=true",
                &session,
            ))
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let all: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 1);

        let response = app
            .clone()
            .oneshot(request_json(
                "POST",
                &format!("/api/v1/sources/{}/scan", source.id),
                &session,
                &csrf,
                String::new(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = app.oneshot(patch(r#"{"archived":false}"#)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_non_loopback_host() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state(dir.path()));
        let request = Request::builder()
            .uri("/api/v1/health")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn range_parser_caps_chunks_and_rejects_invalid() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=990-", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=-10", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=0-99999999", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 1000), None);
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn weak_etag_comparison_supports_lists_and_wildcard() {
        let etag = r#"W/"10-20""#;
        assert!(if_none_match_matches(etag, etag));
        assert!(if_none_match_matches(r#""x", W/"10-20""#, etag));
        assert!(if_none_match_matches("*", etag));
        assert!(!if_none_match_matches(r#""other""#, etag));
    }

    #[test]
    fn if_range_matches_etags_and_dates() {
        let etag = r#"W/"10-20""#;
        assert!(if_range_matches(etag, etag, 1_700_000_000));
        assert!(!if_range_matches(r#"W/"10-21""#, etag, 1_700_000_000));

        let date = http_date(1_700_000_000).unwrap();
        assert!(if_range_matches(&date, etag, 1_699_999_999));
        assert!(if_range_matches(&date, etag, 1_700_000_000));
        assert!(!if_range_matches(&date, etag, 1_700_000_001));
        assert!(!if_range_matches("not a date", etag, 1_700_000_000));
    }

    #[test]
    fn if_modified_since_compares_whole_seconds() {
        let date = http_date(1_700_000_000).unwrap();
        assert!(if_modified_since_not_modified(&date, 1_700_000_000));
        assert!(if_modified_since_not_modified(&date, 1_699_999_999));
        assert!(!if_modified_since_not_modified(&date, 1_700_000_001));
        assert!(!if_modified_since_not_modified(&date, 0));
    }
}
