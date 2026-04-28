use super::error::ApiError;
use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// Union `data` arrays from every provider, dedup by `id`. For each entry,
/// overlay our static `ProviderInfo`/`ModelInfo` metadata under
/// `"x_llm_router"` so OpenAI-shape stays intact for legacy clients while
/// richer consumers (TUIs, dashboards) can pick up capabilities/costs/limits.
pub async fn list_models(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut last_err: Option<String> = None;

    for acct in s.pool.all() {
        let provider = acct.provider.clone();
        match provider.list_models(&s.http).await {
            Ok(v) => {
                let arr = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
                for mut m in arr {
                    let id = m.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if id.is_empty() || !seen.insert(id.clone()) {
                        continue;
                    }
                    enrich(&mut m, &id, provider.as_ref());
                    out.push(m);
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

/// Attach an `x_llm_router` block describing the provider and (when known)
/// the model's static capability/cost/limit metadata.
fn enrich(entry: &mut Value, id: &str, provider: &dyn crate::provider::Provider) {
    let info = provider.info();
    let mut meta = Map::new();
    meta.insert("provider".into(), json!(info.id));
    meta.insert("provider_display_name".into(), json!(info.display_name));
    meta.insert("auth_kind".into(), serde_json::to_value(info.auth_kind).unwrap_or(Value::Null));

    if let Some(mi) = provider.model_info(id) {
        meta.insert("name".into(), json!(mi.name));
        meta.insert(
            "capabilities".into(),
            serde_json::to_value(&mi.capabilities).unwrap_or(Value::Null),
        );
        if let Some(cost) = &mi.cost {
            meta.insert("cost".into(), serde_json::to_value(cost).unwrap_or(Value::Null));
        }
        meta.insert(
            "limit".into(),
            serde_json::to_value(&mi.limit).unwrap_or(Value::Null),
        );
        if let Some(rd) = &mi.release_date {
            meta.insert("release_date".into(), json!(rd));
        }
    }

    if let Some(obj) = entry.as_object_mut() {
        obj.insert("x_llm_router".into(), Value::Object(meta));
    }
}
