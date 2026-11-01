use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};

use localreel_kernel::time::now_secs;
use localreel_media::{Media, MediaState, Source};

use crate::error::ApiError;
use crate::guard::loopback_guard;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/v1/health", get(health))
        .route("/api/v1/events", get(events))
        .route("/api/v1/sources", get(list_sources).post(add_source))
        .route("/api/v1/media", get(list_media))
        .route("/api/v1/media/:id", get(get_media))
        .route("/api/v1/media/:id/state", get(get_state).put(put_state))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            loopback_guard,
        ))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
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
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_media(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<Media>>, ApiError> {
    let media = state
        .media
        .list_media(query.limit.unwrap_or(50), query.offset.unwrap_or(0))?;
    Ok(Json(media))
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
}
