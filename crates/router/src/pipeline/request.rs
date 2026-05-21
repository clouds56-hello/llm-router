use crate::api::error::ApiError;
use crate::api::AppState;
use crate::pipeline::parse::RequestParser;
use crate::provider::Endpoint;
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use tokn_accounts::{AccountHandle, EndpointAcquire};
use tokn_config::RouteMode;
use tokn_core::pipeline::{ParsedRequest, RequestMeta};
use tokn_core::provider::TemplateVars;
use tracing::warn;

pub struct DryRunOutput {
  pub account_id: String,
  pub provider_id: String,
  pub model: String,
  pub endpoint: Endpoint,
  pub headers: reqwest::header::HeaderMap,
  pub body: Bytes,
}

#[derive(Clone, Copy)]
pub enum DryRunEndpoint {
  ChatCompletions,
  Responses,
  Messages,
}

impl From<DryRunEndpoint> for Endpoint {
  fn from(value: DryRunEndpoint) -> Self {
    match value {
      DryRunEndpoint::ChatCompletions => Endpoint::ChatCompletions,
      DryRunEndpoint::Responses => Endpoint::Responses,
      DryRunEndpoint::Messages => Endpoint::Messages,
    }
  }
}

struct PreparedDryRun {
  meta: RequestMeta,
  upstream_body: Value,
  debug_outbound_body: Bytes,
  content_encoding: Option<crate::api::codec::ContentEncodingKind>,
  provider_headers: reqwest::header::HeaderMap,
  profile_headers: Option<reqwest::header::HeaderMap>,
  vars: TemplateVars,
  account: Arc<AccountHandle>,
}

fn resolve_request(
  state: &AppState,
  parsed: ParsedRequest,
  attempt: usize,
) -> Result<(RequestMeta, Value, Arc<AccountHandle>, String), ApiError> {
  let route = state
    .route
    .resolve(
      &parsed.meta.model,
      parsed
        .meta
        .inbound_headers
        .get(tokn_accounts::routing::RouteResolver::mode_header())
        .map(|v| v.as_str()),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
  if matches!(route.mode, RouteMode::Passthrough | RouteMode::Switch) {
    return Err(ApiError::bad_request(format!(
      "{} mode only applies in proxy mode",
      match route.mode {
        RouteMode::Passthrough => "passthrough",
        RouteMode::Switch => "switch",
        _ => unreachable!(),
      }
    )));
  }
  let (account, upstream_endpoint) =
    match state
      .pool
      .acquire_for_route(parsed.meta.session_id.as_deref(), &route, parsed.meta.endpoint)
    {
      EndpointAcquire::Account { acct, endpoint } => (acct, endpoint),
      EndpointAcquire::SessionExpired => {
        let id = parsed.meta.session_id.clone().unwrap_or_default();
        warn!(%parsed.meta.endpoint, model = %parsed.meta.model, session_id = %id, attempt, "session expired");
        return Err(ApiError::session_expired(id));
      }
      EndpointAcquire::None => {
        warn!(%parsed.meta.endpoint, model = %parsed.meta.model, attempt, "no account supports endpoint/model");
        return Err(ApiError::not_implemented(
          parsed.meta.endpoint.to_string(),
          parsed.meta.model.clone(),
        ));
      }
    };

  let mut meta = parsed.meta;
  meta.upstream_endpoint = upstream_endpoint;
  meta.upstream_model = route.upstream_model.clone();
  Ok((meta, parsed.body, account, route.upstream_model))
}

fn prepare_dry_run(
  meta: RequestMeta,
  body: Value,
  account: Arc<AccountHandle>,
  raw_body: Bytes,
  content_encoding: Option<crate::api::codec::ContentEncodingKind>,
) -> crate::provider::Result<PreparedDryRun> {
  let mut upstream_body = rewrite_model(&body, &meta.upstream_model);
  if meta.upstream_endpoint != meta.endpoint {
    upstream_body =
      crate::convert::convert_request(meta.endpoint, meta.upstream_endpoint, &upstream_body).map_err(|source| {
        crate::provider::error::Error::Profiles {
          message: format!("request conversion failed: {source}"),
        }
      })?;
  }
  if let Some(transformer) = account.provider.input_transformer() {
    upstream_body = transformer.transform_input(meta.upstream_endpoint, upstream_body)?;
  }
  let debug_outbound_body = Bytes::from(serde_json::to_vec(&upstream_body).unwrap_or_default());
  let _upstream_wire_body = if upstream_body == body {
    raw_body
  } else {
    crate::api::codec::encode_body_bytes(debug_outbound_body.as_ref(), content_encoding)
      .map_err(|message| crate::provider::error::Error::Profiles { message })?
  };
  let inbound_compat: reqwest::header::HeaderMap = meta.inbound_headers.clone().into();
  let provider_headers = provider_headers(&inbound_compat);
  let vars = crate::proxy::header_pipeline::parse_inbound_vars(&inbound_compat);
  let profile_headers = build_profile_headers(&meta, &account, &inbound_compat, &vars);
  Ok(PreparedDryRun {
    meta,
    upstream_body,
    debug_outbound_body,
    content_encoding,
    provider_headers,
    profile_headers,
    vars,
    account,
  })
}

fn build_profile_headers(
  meta: &RequestMeta,
  account: &Arc<AccountHandle>,
  inbound: &reqwest::header::HeaderMap,
  vars: &TemplateVars,
) -> Option<reqwest::header::HeaderMap> {
  let persona = selected_persona(meta, account)?;
  crate::proxy::header_pipeline::build_headers(crate::proxy::header_pipeline::HeaderPipelineInput {
    profiles: tokn_config::profiles::Profiles::global(),
    persona: persona.as_str(),
    provider_id: account.provider.info().id.as_str(),
    inbound,
    provider_patch: Some(&account_extra_headers(&account.config.load().headers)),
    vars,
  })
  .map(|out| out.headers)
}

fn selected_persona(meta: &RequestMeta, account: &Arc<AccountHandle>) -> Option<String> {
  meta
    .behave_as
    .clone()
    .or_else(|| settings_behave_as(&account.config.load().settings))
    .or_else(|| default_persona(account.provider.info().id.as_str()).map(str::to_string))
}

fn settings_behave_as(settings: &toml::Table) -> Option<String> {
  settings
    .get("behave_as")
    .and_then(|v| v.as_str())
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(ToOwned::to_owned)
}

fn default_persona(provider_id: &str) -> Option<&'static str> {
  match provider_id {
    crate::provider::ID_CODEX => Some("codex"),
    crate::provider::ID_DEEPSEEK | crate::provider::ID_LLAMA_CPP | crate::provider::ID_OPENAI => Some("opencode"),
    crate::provider::ID_GITHUB_COPILOT => Some("copilot"),
    crate::provider::ID_ZAI
    | crate::provider::ID_ZAI_CODING_PLAN
    | crate::provider::ID_ZHIPUAI
    | crate::provider::ID_ZHIPUAI_CODING_PLAN => Some("opencode"),
    _ => None,
  }
}

fn account_extra_headers(headers: &std::collections::BTreeMap<String, String>) -> reqwest::header::HeaderMap {
  let mut out = reqwest::header::HeaderMap::new();
  for (name, value) in headers {
    if tokn_config::profiles::is_router_controlled(name) {
      continue;
    }
    let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
      continue;
    };
    let Ok(value) = reqwest::header::HeaderValue::from_str(value) else {
      continue;
    };
    out.insert(name, value);
  }
  out
}

fn rewrite_model(body: &Value, model: &str) -> Value {
  let mut body = body.clone();
  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".into(), Value::String(model.to_string()));
  }
  body
}

pub fn dry_run_request(
  state: &AppState,
  endpoint: DryRunEndpoint,
  headers: reqwest::header::HeaderMap,
  body: Value,
  raw_body: Bytes,
  content_encoding: Option<crate::api::codec::ContentEncodingKind>,
) -> Result<DryRunOutput, ApiError> {
  let parsed = match endpoint {
    DryRunEndpoint::ChatCompletions => crate::pipeline::ChatParser.parse(headers, body),
    DryRunEndpoint::Responses => crate::pipeline::ResponsesParser.parse(headers, body),
    DryRunEndpoint::Messages => crate::pipeline::MessagesParser.parse(headers, body),
  };
  let (meta, body, account, _) = resolve_request(state, parsed, 0)?;
  let prepared = prepare_dry_run(meta, body, account, raw_body, content_encoding)
    .map_err(|e| ApiError::bad_gateway(e.to_string()))?;
  let mut headers: tokn_headers::HeaderMap = prepared.profile_headers.as_ref().map(|h| h.into()).unwrap_or_default();
  let inbound_lh: tokn_headers::HeaderMap = (&prepared.provider_headers).into();
  prepared
    .account
    .provider
    .patch_headers(
      &mut headers,
      &crate::provider::HeaderPatchCtx {
        endpoint: prepared.meta.upstream_endpoint,
        body: &prepared.upstream_body,
        bearer_token: None,
        content_encoding: prepared.content_encoding.map(|encoding| encoding.as_str()),
        stream: prepared.meta.stream,
        initiator: prepared.meta.initiator.as_str(),
        inbound_headers: &inbound_lh,
        vars: &prepared.vars,
      },
    )
    .ok();
  let headers: reqwest::header::HeaderMap = headers.into();
  Ok(DryRunOutput {
    account_id: prepared.account.id(),
    provider_id: prepared.account.provider.info().id.clone(),
    model: prepared.meta.upstream_model,
    endpoint: prepared.meta.upstream_endpoint,
    headers,
    body: prepared.debug_outbound_body,
  })
}

fn provider_headers(headers: &reqwest::header::HeaderMap) -> reqwest::header::HeaderMap {
  headers
    .iter()
    .filter(|(name, _)| !crate::api::is_router_owned_header(name))
    .map(|(name, value)| (name.clone(), value.clone()))
    .collect()
}
