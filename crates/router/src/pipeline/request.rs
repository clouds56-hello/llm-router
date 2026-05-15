use crate::accounts::{AccountHandle, EndpointAcquire};
use crate::api::error::ApiError;
use crate::api::routing::RouteResolution;
use crate::api::AppState;
use crate::pipeline::parse::RequestParser;
use crate::provider::{new_outbound_capture, Endpoint, RequestCtx};
use bytes::Bytes;
use llm_config::RouteMode;
use llm_core::pipeline::{ParsedRequest, RequestMeta, RequestResolver, RequestSender};
use llm_core::provider::TemplateVars;
use serde_json::Value;
use std::sync::Arc;
use tracing::warn;

#[derive(Clone)]
pub(crate) struct ResolvedRequest {
  pub(crate) meta: RequestMeta,
  pub(crate) body: Value,
  pub(crate) raw_body: Bytes,
  /// Post-decompression bytes of the inbound request body.
  pub(crate) decoded_body: Bytes,
  pub(crate) content_encoding: Option<crate::api::codec::ContentEncodingKind>,
  pub(crate) route: RouteResolution,
  pub(crate) account: Arc<AccountHandle>,
}

pub(super) struct PreparedRequest {
  pub(super) meta: RequestMeta,
  pub(super) inbound_body: Value,
  /// Post-decompression bytes of the inbound request body. Cheap to clone.
  pub(super) inbound_body_bytes: Bytes,
  pub(super) upstream_body: Value,
  pub(super) upstream_wire_body: Bytes,
  pub(super) debug_outbound_body: Bytes,
  pub(super) content_encoding: Option<crate::api::codec::ContentEncodingKind>,
  pub(super) provider_headers: reqwest::header::HeaderMap,
  pub(super) profile_headers: Option<reqwest::header::HeaderMap>,
  pub(super) vars: TemplateVars,
  pub(super) account: Arc<AccountHandle>,
  pub(super) capture: crate::provider::OutboundCapture,
}

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

pub(super) struct PoolResolver;
pub(super) struct ProviderSender;

impl RequestResolver for PoolResolver {
  type State = AppState;
  type Resolved = ResolvedRequest;
  type Error = ApiError;

  fn resolve(&self, state: &AppState, parsed: ParsedRequest, attempt: usize) -> Result<ResolvedRequest, ApiError> {
    resolve_request(state, parsed, attempt)
  }
}

impl RequestSender for ProviderSender {
  type State = AppState;
  type Request = PreparedRequest;
  type Response = reqwest::Response;
  type Error = crate::provider::error::Error;

  fn send<'a>(
    &'a self,
    state: &'a AppState,
    req: &'a PreparedRequest,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::provider::Result<reqwest::Response>> + Send + 'a>> {
    Box::pin(send_request(state, req))
  }
}

fn resolve_request(state: &AppState, parsed: ParsedRequest, attempt: usize) -> Result<ResolvedRequest, ApiError> {
  let route = state
    .route
    .resolve(
      &parsed.meta.model,
      parsed
        .meta
        .inbound_headers
        .get(crate::api::routing::RouteResolver::mode_header())
        .and_then(|v| v.to_str().ok()),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
  if route.mode == RouteMode::Passthrough {
    return Err(ApiError::bad_request("passthrough mode only applies in proxy mode"));
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
  Ok(ResolvedRequest {
    meta,
    body: parsed.body,
    raw_body: Bytes::new(),
    decoded_body: Bytes::new(),
    content_encoding: None,
    route,
    account,
  })
}

pub(super) fn prepare_request(req: ResolvedRequest) -> crate::provider::Result<PreparedRequest> {
  let mut upstream_body = rewrite_model(&req.body, &req.meta.upstream_model);
  if req.meta.upstream_endpoint != req.meta.endpoint {
    upstream_body = crate::convert::convert_request(req.meta.endpoint, req.meta.upstream_endpoint, &upstream_body)
      .map_err(|source| crate::provider::error::Error::Profiles {
        message: format!("request conversion failed: {source}"),
      })?;
  }
  if let Some(transformer) = req.account.provider.input_transformer() {
    upstream_body = transformer.transform_input(&req.meta, upstream_body)?;
  }
  let debug_outbound_body = Bytes::from(serde_json::to_vec(&upstream_body).unwrap_or_default());
  let upstream_wire_body = if upstream_body == req.body {
    req.raw_body.clone()
  } else {
    crate::api::codec::encode_body_bytes(debug_outbound_body.as_ref(), req.content_encoding)
      .map_err(|message| crate::provider::error::Error::Profiles { message })?
  };
  let provider_headers = provider_headers(&req.meta.inbound_headers);
  let vars = crate::proxy::header_pipeline::parse_inbound_vars(&req.meta.inbound_headers);
  let profile_headers = build_profile_headers(&req, &req.meta.inbound_headers, &vars);
  Ok(PreparedRequest {
    meta: req.meta,
    inbound_body: req.body,
    inbound_body_bytes: req.decoded_body,
    upstream_body,
    upstream_wire_body,
    debug_outbound_body,
    content_encoding: req.content_encoding,
    provider_headers,
    profile_headers,
    vars,
    account: req.account,
    capture: new_outbound_capture(),
  })
}

async fn send_request(state: &AppState, req: &PreparedRequest) -> crate::provider::Result<reqwest::Response> {
  let ctx = RequestCtx {
    endpoint: req.meta.upstream_endpoint,
    http: &state.http,
    body: &req.upstream_body,
    body_bytes: Some(&req.upstream_wire_body),
    content_encoding: req.content_encoding.map(|encoding| encoding.as_str()),
    stream: req.meta.stream,
    initiator: req.meta.initiator.as_str(),
    inbound_headers: &req.provider_headers,
    behave_as: req.meta.behave_as.as_deref(),
    profile_headers: req.profile_headers.clone(),
    outbound: Some(req.capture.clone()),
    vars: req.vars.clone(),
  };
  match req.meta.upstream_endpoint {
    Endpoint::ChatCompletions => req.account.provider.chat(ctx).await,
    Endpoint::Responses => req.account.provider.responses(ctx).await,
    Endpoint::Messages => req.account.provider.messages(ctx).await,
  }
}

fn build_profile_headers(
  req: &ResolvedRequest,
  inbound: &reqwest::header::HeaderMap,
  vars: &TemplateVars,
) -> Option<reqwest::header::HeaderMap> {
  let persona = selected_persona(req)?;
  crate::proxy::header_pipeline::build_headers(crate::proxy::header_pipeline::HeaderPipelineInput {
    profiles: llm_config::profiles::Profiles::global(),
    persona: persona.as_str(),
    provider_id: req.account.provider.info().id.as_str(),
    inbound,
    provider_patch: Some(&account_extra_headers(&req.account.config.load().headers)),
    vars,
  })
  .map(|out| out.headers)
}

fn selected_persona(req: &ResolvedRequest) -> Option<String> {
  req
    .meta
    .behave_as
    .clone()
    .or_else(|| settings_behave_as(&req.account.config.load().settings))
    .or_else(|| default_persona(req.account.provider.info().id.as_str()).map(str::to_string))
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
    crate::provider::ID_DEEPSEEK | crate::provider::ID_OPENAI => Some("opencode"),
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
    if llm_config::profiles::is_router_controlled(name) {
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
  decoded_body: Bytes,
  raw_body: Bytes,
  content_encoding: Option<crate::api::codec::ContentEncodingKind>,
) -> Result<DryRunOutput, ApiError> {
  let parsed = match endpoint {
    DryRunEndpoint::ChatCompletions => crate::pipeline::ChatParser.parse(headers, body),
    DryRunEndpoint::Responses => crate::pipeline::ResponsesParser.parse(headers, body),
    DryRunEndpoint::Messages => crate::pipeline::MessagesParser.parse(headers, body),
  };
  let mut resolved = resolve_request(state, parsed, 0)?;
  resolved.raw_body = raw_body;
  resolved.decoded_body = decoded_body;
  resolved.content_encoding = content_encoding;
  let prepared = prepare_request(resolved).map_err(|e| ApiError::bad_gateway(e.to_string()))?;
  let mut headers = prepared.profile_headers.clone().unwrap_or_default();
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
        inbound_headers: &prepared.provider_headers,
        vars: &prepared.vars,
      },
    )
    .ok();
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
