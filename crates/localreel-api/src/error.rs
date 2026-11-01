use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use localreel_kernel::error::KernelError;

/// HTTP-facing error with a stable JSON envelope:
/// `{"error":{"code":"...","message":"..."}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
}

impl From<KernelError> for ApiError {
    fn from(error: KernelError) -> Self {
        let status = match &error {
            KernelError::NotFound(_) => StatusCode::NOT_FOUND,
            KernelError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            KernelError::PermissionDenied(_) => StatusCode::FORBIDDEN,
            KernelError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            KernelError::Database(_)
            | KernelError::Io(_)
            | KernelError::Storage(_)
            | KernelError::Job(_)
            | KernelError::Media(_)
            | KernelError::LockPoisoned(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let code = match status {
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::FORBIDDEN => "forbidden",
            _ => "internal",
        };
        Self::new(status, code, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                }
            })),
        )
            .into_response()
    }
}
