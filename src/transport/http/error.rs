use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;

#[derive(Serialize, Debug)]
pub(super) struct ErrorResponse {
    pub(super) error: String,
}

/// Typed HTTP error mapped cleanly to JSON `{"error": "..."}` responses.
#[derive(Debug)]
pub(super) enum ApiError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    UnsupportedMediaType(String),
    ServiceUnavailable(String),
    Internal(String),
}

impl ApiError {
    pub(super) fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub(super) fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }

    pub(super) fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    pub(super) fn unsupported_media_type(msg: impl Into<String>) -> Self {
        Self::UnsupportedMediaType(msg.into())
    }

    pub(super) fn service_unavailable(msg: impl Into<String>) -> Self {
        Self::ServiceUnavailable(msg.into())
    }

    pub(super) fn internal() -> Self {
        Self::Internal("internal server error".to_string())
    }

    pub(super) fn internal_with_message(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub(super) fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    #[cfg(test)]
    pub(super) fn message(&self) -> &str {
        match self {
            Self::BadRequest(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::UnsupportedMediaType(msg)
            | Self::ServiceUnavailable(msg)
            | Self::Internal(msg) => msg.as_str(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let error = match self {
            Self::BadRequest(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::UnsupportedMediaType(msg)
            | Self::ServiceUnavailable(msg)
            | Self::Internal(msg) => msg,
        };
        (status, Json(ErrorResponse { error })).into_response()
    }
}
