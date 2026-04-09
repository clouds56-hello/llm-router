use async_trait::async_trait;
use chrono::Utc;
use futures::stream;
use reqwest::Client;
use router_core::{
  ChatStream, OpenAiChatChoice, OpenAiChatCompletionRequest, OpenAiChatCompletionResponse, OpenAiChatMessage,
  OpenAiEmbeddingRequest, OpenAiEmbeddingResponse, OpenAiModelList, ProviderAdapter, ProviderRequestMeta,
  RequestContext, RouterError,
};

pub struct OpenAiAdapter {
  provider_name: String,
  base_url: String,
  api_key: String,
  client: Client,
}

impl OpenAiAdapter {
  pub fn new(provider_name: String, base_url: String, api_key: String) -> Self {
    Self {
      provider_name,
      base_url,
      api_key,
      client: Client::new(),
    }
  }
}

#[async_trait]
impl ProviderAdapter for OpenAiAdapter {
  fn provider_name(&self) -> &str {
    &self.provider_name
  }

  async fn send_chat(
    &self,
    _ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<OpenAiChatCompletionResponse, RouterError> {
    let mut payload =
      serde_json::to_value(req).map_err(|e| RouterError::Internal(format!("serialize chat request failed: {}", e)))?;
    payload["model"] = meta.provider_model.clone().into();
    payload["stream"] = false.into();

    let resp = self
      .client
      .post(format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/')))
      .bearer_auth(&self.api_key)
      .json(&payload)
      .send()
      .await
      .map_err(|e| RouterError::Upstream(e.to_string()))?;

    if !resp.status().is_success() {
      let body = resp.text().await.unwrap_or_default();
      return Err(RouterError::Upstream(format!(
        "{} chat failed: {}",
        self.provider_name, body
      )));
    }

    resp
      .json::<OpenAiChatCompletionResponse>()
      .await
      .map_err(|e| RouterError::Upstream(format!("decode chat response failed: {}", e)))
  }

  async fn send_chat_stream(
    &self,
    ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<ChatStream, RouterError> {
    // v1 baseline: fallback streaming by chunking a successful non-stream response.
    let resp = self.send_chat(ctx, req, meta).await?;
    let content = resp
      .choices
      .first()
      .map(|c| c.message.content.clone())
      .unwrap_or_default();

    let first = router_core::RouterError::simple_chunk(&req.model, &content, None);
    let done = router_core::RouterError::simple_chunk(&req.model, "", Some("stop"));
    Ok(Box::pin(stream::iter(vec![Ok(first), Ok(done)])))
  }

  async fn send_embeddings(
    &self,
    _ctx: &RequestContext,
    req: &OpenAiEmbeddingRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<OpenAiEmbeddingResponse, RouterError> {
    let mut payload = serde_json::to_value(req)
      .map_err(|e| RouterError::Internal(format!("serialize embeddings request failed: {}", e)))?;
    payload["model"] = meta.provider_model.clone().into();

    let resp = self
      .client
      .post(format!("{}/v1/embeddings", self.base_url.trim_end_matches('/')))
      .bearer_auth(&self.api_key)
      .json(&payload)
      .send()
      .await
      .map_err(|e| RouterError::Upstream(e.to_string()))?;

    if !resp.status().is_success() {
      let body = resp.text().await.unwrap_or_default();
      return Err(RouterError::Upstream(format!(
        "{} embeddings failed: {}",
        self.provider_name, body
      )));
    }

    resp
      .json::<OpenAiEmbeddingResponse>()
      .await
      .map_err(|e| RouterError::Upstream(format!("decode embeddings response failed: {}", e)))
  }

  async fn list_models(&self, _ctx: &RequestContext) -> Result<OpenAiModelList, RouterError> {
    let resp = self
      .client
      .get(format!("{}/v1/models", self.base_url.trim_end_matches('/')))
      .bearer_auth(&self.api_key)
      .send()
      .await
      .map_err(|e| RouterError::Upstream(e.to_string()))?;

    if !resp.status().is_success() {
      return Ok(OpenAiModelList {
        object: "list".to_string(),
        data: vec![router_core::OpenAiModelData {
          id: format!("{}-fallback", self.provider_name),
          object: "model".to_string(),
          created: Utc::now().timestamp(),
          owned_by: self.provider_name.clone(),
        }],
      });
    }

    resp
      .json::<OpenAiModelList>()
      .await
      .map_err(|e| RouterError::Upstream(format!("decode model list failed: {}", e)))
  }
}

pub fn fallback_response(model: &str, content: &str) -> OpenAiChatCompletionResponse {
  OpenAiChatCompletionResponse {
    id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
    object: "chat.completion".to_string(),
    created: Utc::now().timestamp(),
    model: model.to_string(),
    choices: vec![OpenAiChatChoice {
      index: 0,
      message: OpenAiChatMessage {
        role: "assistant".to_string(),
        content: content.to_string(),
      },
      finish_reason: Some("stop".to_string()),
    }],
    usage: None,
  }
}
