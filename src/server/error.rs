use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub kind: &'static str,
}

impl ApiError {
    pub fn upstream(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            message: body.into(),
            kind: "upstream_error",
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            kind: "internal_error",
        }
    }
    #[allow(dead_code)]
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            kind: "bad_request",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "code": self.status.as_u16(),
            }
        }));
        (self.status, body).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self { ApiError::internal(e.to_string()) }
}
