use std::sync::{
  atomic::{AtomicUsize, Ordering},
  Arc,
};

use async_trait::async_trait;
use futures::stream;
use router_core::{
  AdapterRegistry, ChatStream, ListenerConfig, ModelMapping, OpenAiChatChoice, OpenAiChatCompletionRequest,
  OpenAiChatCompletionResponse, OpenAiChatMessage, OpenAiEmbeddingRequest, OpenAiEmbeddingResponse, OpenAiModelData,
  OpenAiModelList, ProviderAdapter, ProviderConfig, ProviderKind, ProviderRequestMeta, RequestContext, ResolvedConfig,
  ResolvedProvider, RouteConfig, RouterConfig, RouterEngine, RouterError, ViewerConfig,
};

struct TestAdapter {
  name: String,
  fail_chat: bool,
  call_count: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderAdapter for TestAdapter {
  fn provider_name(&self) -> &str {
    &self.name
  }

  async fn send_chat(
    &self,
    _ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    _meta: &ProviderRequestMeta,
  ) -> Result<OpenAiChatCompletionResponse, RouterError> {
    self.call_count.fetch_add(1, Ordering::SeqCst);
    if self.fail_chat {
      return Err(RouterError::Upstream("boom".to_string()));
    }
    Ok(OpenAiChatCompletionResponse {
      id: "chatcmpl-test".to_string(),
      object: "chat.completion".to_string(),
      created: 1,
      model: req.model.clone(),
      choices: vec![OpenAiChatChoice {
        index: 0,
        message: OpenAiChatMessage {
          role: "assistant".to_string(),
          content: "ok".to_string(),
        },
        finish_reason: Some("stop".to_string()),
      }],
      usage: None,
    })
  }

  async fn send_chat_stream(
    &self,
    _ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    _meta: &ProviderRequestMeta,
  ) -> Result<ChatStream, RouterError> {
    let chunk = RouterError::simple_chunk(&req.model, "ok", Some("stop"));
    Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
  }

  async fn send_embeddings(
    &self,
    _ctx: &RequestContext,
    _req: &OpenAiEmbeddingRequest,
    _meta: &ProviderRequestMeta,
  ) -> Result<OpenAiEmbeddingResponse, RouterError> {
    Err(RouterError::BadRequest("not needed".to_string()))
  }

  async fn list_models(&self, _ctx: &RequestContext) -> Result<OpenAiModelList, RouterError> {
    Ok(OpenAiModelList {
      object: "list".to_string(),
      data: vec![OpenAiModelData {
        id: self.name.clone(),
        object: "model".to_string(),
        created: 1,
        owned_by: self.name.clone(),
      }],
    })
  }
}

fn mk_resolved() -> ResolvedConfig {
  let raw = RouterConfig {
    api_version: "v1".to_string(),
    listener: ListenerConfig {
      bind: "127.0.0.1:8787".to_string(),
    },
    providers: vec![],
    routes: vec![RouteConfig {
      name: "chat".to_string(),
      model_aliases: vec!["gpt-4o".to_string()],
      failover_chain: vec!["primary".to_string(), "secondary".to_string()],
      retry_budget: 2,
      circuit_open_after_failures: 1,
    }],
    model_mappings: vec![ModelMapping {
      public_model: "gpt-4o".to_string(),
      provider: "primary".to_string(),
      provider_model: "upstream-gpt-4o".to_string(),
    }],
    viewer: ViewerConfig {
      enabled: false,
      path_prefix: "/viewer".to_string(),
      sqlite_path: ":memory:".to_string(),
      max_rows: 1000,
      max_age_seconds: 3600,
    },
  };

  let mut providers = std::collections::HashMap::new();
  providers.insert(
    "primary".to_string(),
    ResolvedProvider {
      config: ProviderConfig {
        name: "primary".to_string(),
        kind: ProviderKind::OpenAi,
        base_url: "http://localhost".to_string(),
        api_key_env: "X".to_string(),
        timeout_ms: 1000,
        backoff_ms: 1000,
      },
      api_key: "k1".to_string(),
    },
  );
  providers.insert(
    "secondary".to_string(),
    ResolvedProvider {
      config: ProviderConfig {
        name: "secondary".to_string(),
        kind: ProviderKind::OpenAi,
        base_url: "http://localhost".to_string(),
        api_key_env: "Y".to_string(),
        timeout_ms: 1000,
        backoff_ms: 1000,
      },
      api_key: "k2".to_string(),
    },
  );

  let mut model_map = std::collections::HashMap::new();
  model_map.insert(
    "gpt-4o".to_string(),
    ModelMapping {
      public_model: "gpt-4o".to_string(),
      provider: "primary".to_string(),
      provider_model: "upstream-gpt-4o".to_string(),
    },
  );

  ResolvedConfig {
    raw,
    providers,
    model_map,
  }
}

#[tokio::test]
async fn failover_moves_to_secondary() {
  let mut reg = AdapterRegistry::new();
  let primary_calls = Arc::new(AtomicUsize::new(0));
  let secondary_calls = Arc::new(AtomicUsize::new(0));

  reg.register(
    "primary".to_string(),
    Arc::new(TestAdapter {
      name: "primary".to_string(),
      fail_chat: true,
      call_count: primary_calls.clone(),
    }),
  );
  reg.register(
    "secondary".to_string(),
    Arc::new(TestAdapter {
      name: "secondary".to_string(),
      fail_chat: false,
      call_count: secondary_calls.clone(),
    }),
  );

  let engine = RouterEngine::new(mk_resolved(), Arc::new(reg));
  let mut ctx = RequestContext::new(None);
  let req = OpenAiChatCompletionRequest {
    model: "gpt-4o".to_string(),
    messages: vec![OpenAiChatMessage {
      role: "user".to_string(),
      content: "hello".to_string(),
    }],
    stream: false,
    extra: Default::default(),
  };

  let out = engine.route_chat(&mut ctx, &req).await.expect("chat ok");
  assert_eq!(out.choices[0].message.content, "ok");
  assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
  assert_eq!(secondary_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn openai_error_envelope_has_expected_type() {
  let err = RouterError::BadRequest("invalid input".to_string());
  let envelope = err.as_openai_error();
  assert_eq!(envelope.error.type_name, "invalid_request_error");
  assert_eq!(err.status_code(), 400);
}

#[test]
fn config_requires_provider_env_key() {
  let cfg = RouterConfig {
    api_version: "v1".to_string(),
    listener: ListenerConfig {
      bind: "127.0.0.1:8787".to_string(),
    },
    providers: vec![ProviderConfig {
      name: "openai".to_string(),
      kind: ProviderKind::OpenAi,
      base_url: "https://api.openai.com".to_string(),
      api_key_env: "MISSING_ENV_FOR_TEST".to_string(),
      timeout_ms: 1000,
      backoff_ms: 1000,
    }],
    routes: vec![],
    model_mappings: vec![],
    viewer: ViewerConfig {
      enabled: false,
      path_prefix: "/viewer".to_string(),
      sqlite_path: ":memory:".to_string(),
      max_rows: 1000,
      max_age_seconds: 3600,
    },
  };

  std::env::remove_var("MISSING_ENV_FOR_TEST");
  let err = cfg.validate_and_resolve().expect_err("must fail without env");
  assert!(matches!(err, RouterError::Config(_)));
}
