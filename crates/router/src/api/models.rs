use super::error::ApiError;
use super::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use tracing::{debug, instrument};

/// Union `data` arrays from every provider, dedup by `id`. For each entry,
/// overlay our static `ProviderInfo`/`ModelInfo` metadata under
/// `"x_tokn_router"` so OpenAI-shape stays intact for legacy clients while
/// richer consumers (TUIs, dashboards) can pick up capabilities/costs/limits.
#[instrument(name = "list_models", skip_all, fields(accounts = tracing::field::Empty, models = tracing::field::Empty))]
pub async fn list_models(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
  let mut out: Vec<Value> = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();
  let mut last_err: Option<String> = None;

  let accounts = s.pool.all();
  let span = tracing::Span::current();
  span.record("accounts", accounts.len());

  for acct in accounts {
    let provider = acct.provider.clone();
    debug!(account = %acct.id(), provider = %provider.info().id, "list_models: querying account");
    match provider.list_models(&s.http).await {
      Ok(v) => {
        let arr = v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        // Warm the provider's identity cache so `Provider::has_model` can
        // answer accurately for ids that are advertised upstream but not
        // tracked by the catalogue snapshot.
        let cache_ids: HashSet<String> = arr
          .iter()
          .filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(str::to_string))
          .collect();
        if !cache_ids.is_empty() {
          provider.info().model_cache.set(cache_ids);
        }
        let before = out.len();
        for mut m in arr {
          let id = m.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
          if id.is_empty() || !seen.insert(id.clone()) {
            continue;
          }
          enrich(&mut m, &id, provider.as_ref());
          out.push(m);
        }
        debug!(account = %acct.id(), added = out.len() - before, "list_models: account models merged");
      }
      Err(e) => {
        tracing::warn!(account = %acct.id(), error = %e, "list_models failed");
        last_err = Some(e.to_string());
      }
    }
  }

  span.record("models", out.len());

  if out.is_empty() {
    let msg = last_err.unwrap_or_else(|| "no models available".into());
    return Err(ApiError::upstream(StatusCode::BAD_GATEWAY, msg));
  }
  Ok(Json(json!({ "object": "list", "data": out })))
}

/// Attach an `x_tokn_router` block describing the provider and (when known)
/// the model's static capability/cost/limit metadata.
fn enrich(entry: &mut Value, id: &str, provider: &dyn crate::provider::Provider) {
  let info = provider.info();
  let mut meta = Map::new();
  meta.insert("provider".into(), json!(info.id));
  meta.insert("provider_display_name".into(), json!(info.display_name));
  meta.insert(
    "auth_kind".into(),
    serde_json::to_value(info.auth_kind).unwrap_or(Value::Null),
  );

  if let Some(mi) = provider.model_info(id) {
    meta.insert("name".into(), json!(mi.name));
    meta.insert(
      "capabilities".into(),
      serde_json::to_value(&mi.capabilities).unwrap_or(Value::Null),
    );
    if let Some(cost) = &mi.cost {
      meta.insert("cost".into(), serde_json::to_value(cost).unwrap_or(Value::Null));
    }
    meta.insert("limit".into(), serde_json::to_value(&mi.limit).unwrap_or(Value::Null));
    if let Some(rd) = &mi.release_date {
      meta.insert("release_date".into(), json!(rd));
    }
  }

  if let Some(obj) = entry.as_object_mut() {
    obj.insert("x_tokn_router".into(), Value::Object(meta));
  }
}

/// Mode-prefixed variant: `/{mode}/v1/models`
/// TODO: filter models based on route mode (e.g. fuzzy only shows family names)
pub async fn list_models_with_mode(
  State(s): State<AppState>,
  Path(mode): Path<String>,
) -> Result<Json<Value>, ApiError> {
  super::validate_path_mode(&mode)?;
  // For now, delegate to the standard list_models.
  // Future: filter based on mode (e.g. exact mode shows provider/model, fuzzy shows families)
  list_models(State(s)).await
}
