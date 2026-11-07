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
    routing::{get, patch, post},
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

use crate::auth;
use crate::error::ApiError;
use crate::guard::{detail_scope, loopback_guard, security_headers};
use crate::state::AppState;

const MAX_RANGE_CHUNK: u64 = 8 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let thumbnails = ServeDir::new(state.config.media.thumbnail_dir.clone());
    let transcodes = ServeDir::new(state.config.media.transcode_dir.clone());
    let embedded = crate::web::is_embedded();
    let web_dir = state.config.resolved_web_dir();

    // Reachable without a session: the launcher's health probe and the sign-in
    // endpoints themselves.
    let public_api = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/auth/status", get(auth::status))
        .route("/api/v1/auth/setup", post(auth::setup))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/logout", post(auth::logout));

    // Any signed-in account: the shared library plus that account's own state.
    let user_api = Router::new()
        .route("/api/v1/auth/me", get(auth::me))
        .route("/api/v1/media", get(list_media))
        .route("/api/v1/feed", get(feed))
        .route("/api/v1/search", get(search_media))
        .route("/api/v1/media/:id", get(get_media))
        .route("/api/v1/media/:id/state", get(get_state).put(put_state))
        .route("/api/v1/media/:id/playback", post(prepare_playback))
        .route(
            "/api/v1/media/:id/stream",
            get(stream_media).head(head_media),
        )
        .route("/api/v1/events", get(events))
        .nest_service("/thumbnails", thumbnails)
        .nest_service("/transcodes", transcodes)
        .route("/hls/*path", get(serve_hls));

    // Administrators only: library structure, jobs, backups and accounts.
    let admin_api = Router::new()
        .route("/api/v1/admin/health", get(admin_health))
        .route("/api/v1/admin/shutdown", post(shutdown_server))
        .route("/api/v1/admin/cache", get(cache_stats))
        .route("/api/v1/admin/cache/gc", post(run_cache_gc))
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

async fn get_media(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Media>, ApiError> {
    state
        .media
        .get_media(id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))
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

    if is_hls(&media) {
        return Ok(Json(PlaybackResponse {
            mode: "direct",
            url: Some(media.uri),
            job_id: None,
            message: None,
        })
        .into_response());
    }

    if direct_play_supported(&media) {
        return Ok(Json(PlaybackResponse {
            mode: "direct",
            url: Some(format!("/api/v1/media/{}/stream", media.id)),
            job_id: None,
            message: None,
        })
        .into_response());
    }

    let preset_name = preferred_preset(&media);

    if let Some(variant) = state
        .media
        .find_variant(media.id, "transcode", preset_name)?
    {
        let path = state.config.media.transcode_dir.join(&variant.path);
        if path.is_file() {
            if let Err(error) = state.media.touch_variant(media.id, preset_name) {
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
    })
    .to_string();
    // Starting an encode on a machine that is out of memory only makes it
    // slower: say so instead of piling another ffmpeg onto the box.
    if localreel_media::memory_pressure() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "not enough free memory to start a transcode right now",
        ));
    }
    let job_id = state.jobs.enqueue(
        "transcode",
        &payload,
        Some(&format!("transcode:{}:{preset_name}", media.id)),
        2,
    )?;
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

    let payload = serde_json::json!({ "media_id": media.id }).to_string();
    if localreel_media::memory_pressure() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "not enough free memory to package this stream right now",
        ));
    }
    let job_id = state.jobs.enqueue(
        "hls_package",
        &payload,
        Some(&format!("hls:{}", media.id)),
        2,
    )?;
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
    <p><a href="/api/v1/health">Health</a> · <a href="/api/v1/sources">Sources</a> · <a href="/api/v1/media">Media</a></p>
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
        let events = EventBus::new(16);
        let jobs = JobQueue::new(db.clone(), events.clone());
        let media = MediaService::new(db.clone());
        let accounts = localreel_media::AccountService::new(db.clone());
        let transcodes = Arc::new(localreel_media::TranscodeGate::new(
            2,
            std::time::Duration::from_secs(900),
        ));
        AppState::new(crate::state::AppStateParts {
            config: Config::default(),
            db,
            events,
            jobs,
            media,
            accounts,
            transcodes,
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
        let owner = state
            .accounts
            .setup_owner("owner", "correct horse")
            .unwrap();
        let session = state.accounts.create_session(owner.id).unwrap();
        (state, session.id, session.csrf)
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
