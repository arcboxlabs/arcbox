//! Shared REST API types.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Standard error response body.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Convenience alias for handler results.
pub type ApiResult<T> = Result<Json<T>, ApiError>;

/// REST API error that maps to HTTP status codes.
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            error: self.message,
            detail: None,
        };
        (self.status, Json(body)).into_response()
    }
}

impl ApiError {
    pub fn not_implemented(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            message: msg.into(),
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
        }
    }
}

impl From<arcbox_core::CoreError> for ApiError {
    fn from(err: arcbox_core::CoreError) -> Self {
        use arcbox_error::CommonError;
        match err {
            arcbox_core::CoreError::Common(ref common) => match common {
                CommonError::NotFound(_) => Self::not_found(err.to_string()),
                CommonError::AlreadyExists(_) => Self::conflict(err.to_string()),
                CommonError::InvalidState(_) => Self::conflict(err.to_string()),
                CommonError::Config(_) => Self::bad_request(err.to_string()),
                CommonError::PermissionDenied(_) => Self {
                    status: StatusCode::FORBIDDEN,
                    message: err.to_string(),
                },
                CommonError::Timeout(_) => Self {
                    status: StatusCode::GATEWAY_TIMEOUT,
                    message: err.to_string(),
                },
                _ => Self::internal(err.to_string()),
            },
            _ => Self::internal(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_core::CoreError;

    #[test]
    fn core_error_not_found_maps_to_404() {
        let err = CoreError::not_found("machine 'foo'");
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn core_error_already_exists_maps_to_409() {
        let err = CoreError::already_exists("machine 'foo'");
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::CONFLICT);
    }

    #[test]
    fn core_error_invalid_state_maps_to_409() {
        let err = CoreError::invalid_state("already running");
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::CONFLICT);
    }

    #[test]
    fn core_error_config_maps_to_400() {
        let err = CoreError::config("invalid port");
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn core_error_vm_maps_to_500() {
        let err = CoreError::Vm("crash".into());
        let api: ApiError = err.into();
        assert_eq!(api.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn api_error_into_response_has_correct_status() {
        let err = ApiError::not_implemented("not yet");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }
}

