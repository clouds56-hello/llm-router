use super::error::ApiError;
use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Union `data` arrays from every provider, dedup by `id`.
pub async fn list_models(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut last_err: Option<String> = None;

    for acct in s.pool.all() {
        match acct.provider.list_models(&s.http).await {
            Ok(v) => {
                let arr = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
                for m in arr {
                    let id = m.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if id.is_empty() || seen.insert(id) {
                        out.push(m);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(account = %acct.id, error = %e, "list_models failed");
                last_err = Some(e.to_string());
            }
        }
    }

    if out.is_empty() {
        let msg = last_err.unwrap_or_else(|| "no models available".into());
        return Err(ApiError::upstream(StatusCode::BAD_GATEWAY, msg));
    }
    Ok(Json(json!({ "object": "list", "data": out })))
}
