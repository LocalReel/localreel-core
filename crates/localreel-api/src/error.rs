use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use localreel_kernel::error::KernelError;

tokio::task_local! {
    /// Whether the current request may carry diagnostics in an error body.
    ///
    /// Set per request by [`crate::guard::detail_scope`]; absent means "no", so
    /// an error rendered outside a request (a test, a background task) never
    /// leaks detail by accident.
    static REVEAL_DETAIL: bool;
}

/// Run `future` with the request's detail visibility.
pub async fn with_reveal_detail<F>(reveal: bool, future: F) -> F::Output
where
    F: std::future::Future,
{
    REVEAL_DETAIL.scope(reveal, future).await
}

/// Whether the caller of the current response may see diagnostics.
pub fn reveal_detail() -> bool {
    REVEAL_DETAIL.try_with(|reveal| *reveal).unwrap_or(false)
}

/// `RUST_LOG` at debug/trace means somebody is troubleshooting on this machine.
pub fn debug_logging_enabled() -> bool {
    std::env::var("RUST_LOG")
        .map(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("debug") || value.contains("trace")
        })
        .unwrap_or(false)
}

/// HTTP-facing error with a stable JSON envelope:
/// `{"error":{"code":"...","message":"...","detail":"..."}}`.
///
/// `message` is always safe to show; `detail` carries internal diagnostics (an
/// ffmpeg stderr tail, a local path) and is only serialized when the request is
/// allowed to see it — see [`reveal_detail`].
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    detail: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// Attach diagnostics that only administrators may receive.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if !detail.is_empty() {
            self.detail = Some(detail);
        }
        self
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", message)
    }
}

impl From<KernelError> for ApiError {
    fn from(error: KernelError) -> Self {
        let status = match &error {
            KernelError::NotFound(_) => StatusCode::NOT_FOUND,
            KernelError::Conflict(_) => StatusCode::CONFLICT,
            KernelError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            KernelError::PermissionDenied(_) => StatusCode::FORBIDDEN,
            KernelError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            KernelError::Database(_)
            | KernelError::Io(_)
            | KernelError::Storage(_)
            | KernelError::Job(_)
            | KernelError::Media(_)
            | KernelError::MediaDetail { .. }
            | KernelError::LockPoisoned(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let code = match status {
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "conflict",
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::FORBIDDEN => "forbidden",
            _ => "internal",
        };
        Self {
            status,
            code,
            message: error.public_message(),
            detail: error.diagnostic_detail().map(str::to_string),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({
            "code": self.code,
            "message": self.message,
        });
        if let Some(detail) = self.detail.as_deref().filter(|_| reveal_detail()) {
            error["detail"] = json!(detail);
        }
        (self.status, Json(json!({ "error": error }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn failing() -> ApiError {
        ApiError::from(KernelError::media_detail(
            "ffmpeg could not transcode this file",
            "input=C:/library/secret.mkv; stderr: Invalid data found",
        ))
    }

    #[tokio::test]
    async fn a_plain_request_gets_the_message_without_the_detail() {
        let response = failing().into_response();
        let body = body_of(response).await;
        assert_eq!(body["error"]["code"], "internal");
        assert_eq!(
            body["error"]["message"],
            "media operation failed: ffmpeg could not transcode this file"
        );
        assert!(
            body["error"].get("detail").is_none(),
            "an ordinary error body must not carry diagnostics"
        );
        assert!(!body.to_string().contains("secret.mkv"));
    }

    #[tokio::test]
    async fn an_authorised_request_gets_the_detail() {
        let response = with_reveal_detail(true, async { failing().into_response() }).await;
        let body = body_of(response).await;
        let detail = body["error"]["detail"].as_str().unwrap();
        assert!(detail.contains("secret.mkv"));
        assert!(detail.contains("Invalid data found"));
    }

    #[tokio::test]
    async fn a_request_scoped_to_false_hides_the_detail() {
        let response = with_reveal_detail(false, async { failing().into_response() }).await;
        let body = body_of(response).await;
        assert!(body["error"].get("detail").is_none());
    }

    #[test]
    fn the_internal_message_keeps_everything() {
        // Logs and the job history must stay as informative as before.
        let error = KernelError::media_detail("ffmpeg failed", "input=C:/x.mkv");
        assert_eq!(
            error.to_string(),
            "media operation failed: ffmpeg failed: input=C:/x.mkv"
        );
        assert_eq!(
            error.public_message(),
            "media operation failed: ffmpeg failed"
        );
        assert_eq!(error.diagnostic_detail(), Some("input=C:/x.mkv"));
    }

    #[test]
    fn detail_visibility_defaults_to_false() {
        assert!(!reveal_detail());
    }
}
