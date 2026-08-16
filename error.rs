use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// All errors in the daemon go through this enum.
/// `thiserror` generates the `std::error::Error` + `Display` impls.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("docker error: {0}")]
    Docker(#[from] bollard::errors::Error),

    #[error("server not found")]
    ServerNotFound,

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("unprocessable entity: {0}")]
    Unprocessable(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("payload too large")]
    PayloadTooLarge,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("panel request failed: {0}")]
    Remote(String),

    #[error("not implemented: {0}")]
    NotImplemented(String),
}

/// Wings responds to errors with `{"error": "...", "request_id": "..."}`.
/// We keep it close to that shape (the request id is attached by middleware).
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message, log) = match &self {
            AppError::ServerNotFound => {
                (StatusCode::NOT_FOUND, "The requested resource does not exist on this instance.".to_string(), false)
            }
            AppError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, self.to_string(), false),
            AppError::Forbidden(_) => (StatusCode::FORBIDDEN, self.to_string(), false),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string(), false),
            AppError::Unprocessable(_) => (StatusCode::UNPROCESSABLE_ENTITY, self.to_string(), false),
            AppError::Conflict(_) => (StatusCode::CONFLICT, self.to_string(), false),
            AppError::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, self.to_string(), false),
            AppError::Docker(_) | AppError::Io(_) | AppError::Internal(_) | AppError::Config(_) | AppError::Remote(_) | AppError::NotImplemented(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string(), true)
            }
        };

        if log {
            tracing::error!(error = %self, "request failed");
        }

        (status, Json(json!({ "error": message }))).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;