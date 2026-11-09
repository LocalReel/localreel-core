//! Share links over HTTP.
//!
//! Two groups:
//! - signed-in accounts create, list and revoke links for an item;
//! - an unauthenticated request may read exactly one item through its token,
//!   and nothing else. The token is the only key, so it is random, and the
//!   handlers here never accept an id in place of one.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::{Extension, Json};
use localreel_media::CurrentUser;
use serde::Deserialize;

use crate::error::ApiError;
use crate::routes::serve_media;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateShare {
    /// Hours until the link stops working. Omitted means "until revoked".
    pub ttl_hours: Option<i64>,
    pub allow_download: Option<bool>,
}

fn share_json(share: &localreel_media::Share) -> serde_json::Value {
    serde_json::json!({
        "token": share.token,
        "media_id": share.media_id,
        "url": format!("/s/{}", share.token),
        "api": format!("/api/v1/share/{}", share.token),
        "created_at": share.created_at,
        "expires_at": share.expires_at,
        "revoked_at": share.revoked_at,
        "allow_download": share.allow_download,
    })
}

fn active_share(state: &AppState, token: &str) -> Result<localreel_media::Share, ApiError> {
    // One answer for "no such link", "expired" and "revoked": an outsider must
    // not learn which of the three applies.
    let share = state
        .media
        .share_by_token(token)?
        .filter(|share| share.is_active(localreel_kernel::time::now_secs()))
        .ok_or_else(|| ApiError::not_found("share link"))?;
    Ok(share)
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(current): Extension<CurrentUser>,
    Path(media_id): Path<i64>,
    Json(body): Json<CreateShare>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ttl_secs = body.ttl_hours.map(|hours| hours.max(1) * 3600);
    let share = state.media.create_share(
        media_id,
        Some(current.user.id),
        ttl_secs,
        body.allow_download.unwrap_or(false),
    )?;
    Ok(Json(share_json(&share)))
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    Path(media_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let shares = state.media.list_shares(media_id)?;
    Ok(Json(serde_json::json!({
        "items": shares.iter().map(share_json).collect::<Vec<_>>(),
        "total": shares.len(),
    })))
}

pub async fn revoke(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.media.revoke_share(&token)? {
        return Err(ApiError::not_found("share link"));
    }
    Ok(Json(serde_json::json!({ "revoked": true })))
}

/// What a visitor sees about the item behind a link.
pub async fn meta(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let share = active_share(&state, &token)?;
    let media = state
        .media
        .get_media(share.media_id)?
        .ok_or_else(|| ApiError::not_found("share link"))?;
    Ok(Json(serde_json::json!({
        "title": media.title.clone().unwrap_or_else(|| media.filename.clone()),
        "filename": media.filename,
        "duration": media.duration,
        "width": media.width,
        "height": media.height,
        "series_title": media.series_title,
        "episode": media.episode,
        "allow_download": share.allow_download,
        "expires_at": share.expires_at,
        // Relative on purpose: the page works behind a reverse proxy and a
        // LAN address alike.
        "stream": format!("/api/v1/share/{token}/stream"),
    })))
}

/// The bytes. Range handling is the same code the signed-in player uses, so a
/// share cannot accidentally serve something the player would refuse.
pub async fn stream(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let share = active_share(&state, &token)?;
    serve_media(state, share.media_id, headers, false).await
}
