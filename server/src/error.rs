//! Typed server errors and the uniform JSON error body.
//!
//! Every fallible HTTP handler returns [`AppError`]. Its [`IntoResponse`]
//! impl picks the status from the **variant** — not from string-matching
//! the message, which is how the driver-reload 409 used to be detected
//! (`msg.contains("pipeline is running")`) — and renders the same
//! `{ "error": { "code", "message" } }` body every time, so no endpoint
//! answers with a bare plain-text error string.

use axum::{response::IntoResponse, Json};
use http::StatusCode;
use serde::Serialize;

/// Uniform error envelope. `code` is a stable machine token
/// (`SCREAMING_SNAKE_CASE`); `message` is human-facing detail.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
}

/// A typed server error. The variant fixes the HTTP status; callers pick
/// it by meaning (`conflict` for wrong-state, `not_found` for a missing
/// resource, …) instead of hand-writing `(StatusCode, Json<…>)` tuples.
#[derive(Debug)]
pub enum AppError {
    /// 400 — malformed request, or a reconfigure the graph rejected.
    BadRequest { code: &'static str, message: String },
    /// 404 — named resource absent.
    NotFound { code: &'static str, message: String },
    /// 409 — valid request, wrong state (e.g. driver reload while the
    /// pipeline is running).
    Conflict { code: &'static str, message: String },
    /// 503 — a dependency isn't ready (no UI viewer connected, logs
    /// disabled).
    Unavailable { code: &'static str, message: String },
    /// 504 — an upstream (the browser snapshot round-trip) timed out.
    Timeout { code: &'static str, message: String },
    /// 500 — unexpected server fault.
    Internal { code: &'static str, message: String },
}

impl AppError {
    #[must_use]
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::BadRequest {
            code,
            message: message.into(),
        }
    }
    #[must_use]
    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::NotFound {
            code,
            message: message.into(),
        }
    }
    #[must_use]
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }
    #[must_use]
    pub fn unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self::Unavailable {
            code,
            message: message.into(),
        }
    }
    #[must_use]
    pub fn timeout(code: &'static str, message: impl Into<String>) -> Self {
        Self::Timeout {
            code,
            message: message.into(),
        }
    }
    #[must_use]
    pub fn internal(code: &'static str, message: impl Into<String>) -> Self {
        Self::Internal {
            code,
            message: message.into(),
        }
    }

    /// The HTTP status this variant maps to.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::Conflict { .. } => StatusCode::CONFLICT,
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn into_parts(self) -> (StatusCode, &'static str, String) {
        let status = self.status();
        let (code, message) = match self {
            Self::BadRequest { code, message }
            | Self::NotFound { code, message }
            | Self::Conflict { code, message }
            | Self::Unavailable { code, message }
            | Self::Timeout { code, message }
            | Self::Internal { code, message } => (code, message),
        };
        (status, code, message)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, message) = match self {
            Self::BadRequest { code, message }
            | Self::NotFound { code, message }
            | Self::Conflict { code, message }
            | Self::Unavailable { code, message }
            | Self::Timeout { code, message }
            | Self::Internal { code, message } => (code, message),
        };
        write!(f, "{code}: {message}")
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, code, message) = self.into_parts();
        (
            status,
            Json(ApiError {
                error: ApiErrorBody { code, message },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_maps_by_variant() {
        assert_eq!(
            AppError::bad_request("X", "m").status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            AppError::not_found("X", "m").status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(AppError::conflict("X", "m").status(), StatusCode::CONFLICT);
        assert_eq!(
            AppError::unavailable("X", "m").status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            AppError::timeout("X", "m").status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            AppError::internal("X", "m").status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn renders_uniform_json_body() {
        use axum::body::to_bytes;
        let resp = AppError::conflict("RELOAD_REFUSED_RUNNING", "stop it first").into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "RELOAD_REFUSED_RUNNING");
        assert_eq!(v["error"]["message"], "stop it first");
    }
}
