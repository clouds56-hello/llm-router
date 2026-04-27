use super::error::ApiError;
use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;

pub async fn list_models(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let acct = s.pool.acquire();
    let token = acct
        .ensure_api_token(&s.http)
        .await
        .map_err(|e| ApiError::upstream(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let v = crate::copilot::models::list(&s.http, &token, &acct.headers)
        .await
        .map_err(|e| ApiError::upstream(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(v))
}
