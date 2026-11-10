//! A safety net for the admin audit.
//!
//! Handlers record their own actions because only they know the detail worth
//! keeping ("which fields changed", "did the plugin hot-restart"). The cost of
//! that honesty is that a new endpoint forgets to call `record_admin` and
//! silently changes the machine without leaving a row.
//!
//! This middleware watches the mutations under `/api/v1/admin/`: if the request
//! succeeded and nothing was recorded meanwhile, it records the route itself.
//! The "meanwhile" is an explicit counter bumped by `record_admin`, so a handler
//! that did its job produces no duplicate row.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{Method, Request};
use axum::middleware::Next;
use axum::response::Response;

use localreel_media::audit::recorded_count;
use localreel_media::CurrentUser;

use crate::state::AppState;

/// Whether a request is one this middleware should watch.
///
/// Admin mutations only: playback progress and liking a film are mutations too,
/// and auditing them would drown the list that matters.
pub fn should_watch(method: &Method, path: &str) -> bool {
    path.starts_with("/api/v1/admin/")
        && matches!(
            *method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        )
}

/// Did the handler leave a row of its own? If not, the route is recorded.
pub fn needs_fallback(before: u64, after: u64, status: u16) -> bool {
    (200..300).contains(&status) && before == after
}

pub async fn audit_fallback(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let watched = should_watch(request.method(), request.uri().path());
    // Read the actor from the extensions the authentication middleware put
    // there; a public path has none, and there is nothing to attribute.
    let actor = request
        .extensions()
        .get::<CurrentUser>()
        .map(|current| (current.user.id, current.user.username.clone()));
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let before = recorded_count();

    let response = next.run(request).await;

    if watched && needs_fallback(before, recorded_count(), response.status().as_u16()) {
        let (user_id, username) = actor.unwrap_or((0, String::new()));
        let _ = state.media.record_admin(
            (user_id != 0).then_some(user_id),
            (!username.is_empty()).then_some(username.as_str()),
            "admin.api",
            Some(&format!("{} {}", method.as_str(), path)),
            Some("no handler-level audit entry"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_admin_mutations_are_watched() {
        assert!(should_watch(&Method::POST, "/api/v1/admin/shutdown"));
        assert!(should_watch(&Method::PATCH, "/api/v1/admin/config"));
        assert!(should_watch(
            &Method::DELETE,
            "/api/v1/admin/sessions/x/revoke"
        ));
        // Reads never need a row.
        assert!(!should_watch(&Method::GET, "/api/v1/admin/audit"));
        // Personal mutations are not admin actions.
        assert!(!should_watch(&Method::PUT, "/api/v1/media/7/state"));
        assert!(!should_watch(&Method::POST, "/api/v1/auth/login"));
    }

    #[test]
    fn a_handler_that_recorded_gets_no_second_row() {
        assert!(!needs_fallback(5, 6, 200));
        // ...even when it answered with 204.
        assert!(!needs_fallback(5, 6, 204));
    }

    #[test]
    fn a_successful_unrecorded_mutation_gets_a_row() {
        assert!(needs_fallback(5, 5, 200));
        assert!(needs_fallback(5, 5, 204));
    }

    #[test]
    fn a_failed_mutation_records_nothing() {
        assert!(!needs_fallback(5, 5, 400));
        assert!(!needs_fallback(5, 5, 500));
    }
}
