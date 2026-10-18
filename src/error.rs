use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::Serialize;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
        }
    }

    pub fn bad_request(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, detail)
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid bearer token is required.",
        )
    }

    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "asset_not_found",
            "The asset was not found.",
        )
    }

    /// The asset exists, but it holds no rendition at or below the requested
    /// width. This is deliberately a different code from a missing asset: the
    /// upload succeeded, the caller simply asked for something too small.
    pub fn rendition_not_found(max_width: u32, available: &[u32]) -> Self {
        let detail = if available.is_empty() {
            "The asset has no renditions.".to_owned()
        } else {
            format!(
                "The asset has no rendition at or below {max_width} pixels; stored widths are {}.",
                available
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Self::new(StatusCode::NOT_FOUND, "rendition_not_found", detail)
    }

    pub fn not_acceptable() -> Self {
        Self::new(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            "No available representation matches the Accept header.",
        )
    }

    pub fn conflict(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, detail)
    }

    pub fn too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload_too_large",
            "The upload exceeds the configured byte limit.",
        )
    }

    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            detail,
        )
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "processor_unavailable",
            detail,
        )
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_media", detail)
    }

    /// The message a client may safely be shown.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "The request could not be completed.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status;
        let response = ErrorResponse {
            error: ErrorDetail {
                code: self.code,
                message: self.detail,
            },
        };
        let mut response = (status, Json(response)).into_response();
        match status {
            StatusCode::UNAUTHORIZED => {
                response.headers_mut().insert(
                    axum::http::header::WWW_AUTHENTICATE,
                    axum::http::HeaderValue::from_static("Bearer"),
                );
            }
            StatusCode::NOT_ACCEPTABLE => {
                response.headers_mut().insert(
                    axum::http::header::VARY,
                    axum::http::HeaderValue::from_static("Accept"),
                );
            }
            _ => {}
        }
        response
    }
}
