use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tokio_util::io::ReaderStream;
use tower_http::services::ServeDir;

use localreel_kernel::jobs::JobRecord;
use localreel_kernel::storage::MediaUri;
use localreel_kernel::time::now_secs;
use localreel_media::playback::{direct_play_supported, is_hls, DEFAULT_TRANSCODE_PRESET};
use localreel_media::{
    CacheGcReport, CacheStats, Media, MediaFilter, MediaState, ScanIssue, ScanRun, Source,
};

use crate::error::ApiError;
use crate::guard::loopback_guard;
use crate::state::AppState;

const MAX_RANGE_CHUNK: u64 = 8 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let thumbnails = ServeDir::new(state.config.media.thumbnail_dir.clone());
    let transcodes = ServeDir::new(state.config.media.transcode_dir.clone());
    let embedded = crate::web::is_embedded();
    let web_dir = state.config.resolved_web_dir();

    let mut router = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/admin/health", get(admin_health))
        .route("/api/v1/admin/cache", get(cache_stats))
        .route("/api/v1/admin/cache/gc", post(run_cache_gc))
        .route("/api/v1/scan-runs", get(list_scan_runs))
        .route("/api/v1/scan-issues", get(list_scan_issues))
        .route("/api/v1/events", get(events))
        .route("/api/v1/sources", get(list_sources).post(add_source))
        .route("/api/v1/sources/:id", patch(patch_source))
        .route("/api/v1/sources/:id/scan", post(scan_source))
        .route("/api/v1/media", get(list_media))
        .route("/api/v1/search", get(search_media))
        .route("/api/v1/media/:id", get(get_media))
        .route("/api/v1/media/:id/state", get(get_state).put(put_state))
        .route("/api/v1/media/:id/playback", post(prepare_playback))
        .route(
            "/api/v1/media/:id/stream",
            get(stream_media).head(head_media),
        )
        .route("/api/v1/jobs", get(list_jobs))
        .route("/api/v1/jobs/:id", get(get_job))
        .route("/api/v1/jobs/:id/cancel", post(cancel_job))
        .nest_service("/thumbnails", thumbnails)
        .nest_service("/transcodes", transcodes);

    if embedded {
        router = router
            .route("/", get(embedded_index))
            .route("/admin", get(embedded_index))
            .route("/watch", get(embedded_index))
            .route("/assets/*path", get(embedded_asset));
    } else {
        router = router
            .route("/", get(index))
            .route("/admin", get(index))
            .route("/watch", get(index));
        if let Some(web_dir) = web_dir {
            router = router.nest_service("/assets", ServeDir::new(web_dir.join("assets")));
        }
    }

    router
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            loopback_guard,
        ))
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
    status: &'static str,
    version: &'static str,
    database: bool,
    ffmpeg: bool,
    media_count: i64,
    uptime_seconds: i64,
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let database = state.db.health_check().is_ok();
    let media_count = state.media.media_count().unwrap_or(0);
    Json(HealthResponse {
        status: if database { "ok" } else { "degraded" },
        version: localreel_kernel::VERSION,
        database,
        ffmpeg: state.ffmpeg_available,
        media_count,
        uptime_seconds: (now_secs() - state.started_at).max(0),
    })
}

async fn admin_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let database = state.db.health_check().is_ok();
    let media_count = state.media.media_count().unwrap_or(0);
    let recent_jobs = state.jobs.list_recent(20)?;
    Ok(Json(serde_json::json!({
        "status": if database { "ok" } else { "degraded" },
        "version": localreel_kernel::VERSION,
        "database": database,
        "ffmpeg": state.ffmpeg_available,
        "media_count": media_count,
        "data_dir": state.config.data_dir,
        "thumbnail_dir": state.config.media.thumbnail_dir,
        "transcode_dir": state.config.media.transcode_dir,
        "jobs": recent_jobs,
    })))
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

async fn list_sources(State(state): State<Arc<AppState>>) -> Result<Json<Vec<Source>>, ApiError> {
    Ok(Json(state.media.list_sources()?))
}

#[derive(Debug, Deserialize)]
struct AddSourceRequest {
    uri: String,
}

async fn add_source(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AddSourceRequest>,
) -> Result<Response, ApiError> {
    let source = state.media.add_source(&payload.uri)?;
    Ok((StatusCode::CREATED, Json(source)).into_response())
}

#[derive(Debug, Deserialize)]
struct PatchSourceRequest {
    enabled: bool,
}

async fn patch_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(payload): Json<PatchSourceRequest>,
) -> Result<Json<Source>, ApiError> {
    Ok(Json(state.media.set_source_enabled(id, payload.enabled)?))
}

async fn scan_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let source = state
        .media
        .get_source(id)?
        .ok_or_else(|| ApiError::not_found(format!("source {id} not found")))?;
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
}

#[derive(Debug, Serialize)]
struct MediaPage {
    items: Vec<Media>,
    total: i64,
    next_cursor: Option<i64>,
}

async fn list_media(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Json<MediaPage>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let filter = MediaFilter::parse(query.filter.as_deref().unwrap_or("all"))
        .ok_or_else(|| ApiError::bad_request("filter must be all, favorites or continue"))?;
    let (items, total) =
        state
            .media
            .list_media_page(limit, offset, query.source_id, query.q.as_deref(), filter)?;
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
    Query(query): Query<ListQuery>,
) -> Result<Json<MediaPage>, ApiError> {
    list_media(State(state), Query(query)).await
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
    Path(id): Path<i64>,
) -> Result<Json<MediaState>, ApiError> {
    Ok(Json(state.media.get_state(id)?))
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
    Path(id): Path<i64>,
    Json(patch): Json<StatePatch>,
) -> Result<Json<MediaState>, ApiError> {
    let mut current = state.media.get_state(id)?;
    if let Some(value) = patch.liked {
        current.liked = value;
    }
    if let Some(value) = patch.favorite {
        current.favorite = value;
    }
    if let Some(value) = patch.position {
        if !value.is_finite() || value < 0.0 {
            return Err(ApiError::bad_request(
                "position must be finite and non-negative",
            ));
        }
        current.position = value;
    }
    if let Some(value) = patch.duration {
        if !value.is_finite() || value < 0.0 {
            return Err(ApiError::bad_request(
                "duration must be finite and non-negative",
            ));
        }
        current.duration = value;
    }
    if current.duration > 0.0 && current.position > current.duration + 30.0 {
        return Err(ApiError::bad_request("position exceeds duration"));
    }
    Ok(Json(state.media.save_state(&current)?))
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
) -> Result<Response, ApiError> {
    let media = state
        .media
        .get_media(id)?
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))?;
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

    if let Some(variant) =
        state
            .media
            .find_variant(media.id, "transcode", DEFAULT_TRANSCODE_PRESET)?
    {
        let path = state.config.media.transcode_dir.join(&variant.path);
        if path.is_file() {
            if let Err(error) = state
                .media
                .touch_variant(media.id, DEFAULT_TRANSCODE_PRESET)
            {
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
        "preset": DEFAULT_TRANSCODE_PRESET,
    })
    .to_string();
    let job_id = state.jobs.enqueue(
        "transcode",
        &payload,
        Some(&format!(
            "transcode:{}:{DEFAULT_TRANSCODE_PRESET}",
            media.id
        )),
        2,
    )?;
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
    path: PathBuf,
    size: u64,
    etag: String,
    mime: String,
}

async fn resolve_media(state: &AppState, id: i64) -> Result<ResolvedMedia, ApiError> {
    let media = state
        .media
        .get_media(id)?
        .ok_or_else(|| ApiError::not_found(format!("media {id} not found")))?;
    let path = MediaUri::parse(&media.uri)?
        .local_path()
        .map(PathBuf::from)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                "remote media streaming is not supported in v1",
            )
        })?;
    let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found("media file is missing")
        } else {
            ApiError::internal(format!("failed to stat media: {error}"))
        }
    })?;
    if !metadata.is_file() {
        return Err(ApiError::bad_request("media path is not a file"));
    }
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let etag = format!("W/\"{:x}-{:x}\"", metadata.len(), mtime);
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    Ok(ResolvedMedia {
        path,
        size: metadata.len(),
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

    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| if_none_match_matches(value, &media.etag))
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &media.etag)
            .body(Body::empty())
            .map_err(|error| ApiError::internal(error.to_string()));
    }

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let range = range_header.and_then(|value| parse_range(value, media.size));
    if range_header.is_some() && range.is_none() {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", media.size))
            .header(header::ETAG, &media.etag)
            .body(Body::empty())
            .map_err(|error| ApiError::internal(error.to_string()));
    }

    if media.size == 0 {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, &media.mime)
            .header(header::CONTENT_LENGTH, "0")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ETAG, &media.etag)
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
        let mut file = tokio::fs::File::open(&media.path)
            .await
            .map_err(|error| ApiError::internal(format!("failed to open media: {error}")))?;
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(|error| ApiError::internal(format!("failed to seek media: {error}")))?;
        Body::from_stream(ReaderStream::new(file.take(length)))
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, &media.mime)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &media.etag)
        .header(header::CACHE_CONTROL, "private, max-age=3600");
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
        AppState::new(Config::default(), db, events, jobs, media, false)
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("host", "127.0.0.1:3001")
            .body(Body::empty())
            .unwrap()
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
}
