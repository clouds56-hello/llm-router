use async_trait::async_trait;
use chrono::Utc;
use futures::stream;
use reqwest::Client;
use router_core::{
  ChatStream, OpenAiChatChoice, OpenAiChatCompletionRequest, OpenAiChatCompletionResponse, OpenAiChatMessage,
  OpenAiEmbeddingRequest, OpenAiEmbeddingResponse, OpenAiModelData, OpenAiModelList, ProviderAdapter,
  ProviderRequestMeta, RequestContext, RouterError,
};
use serde::{Deserialize, Serialize};

pub struct ClaudeAdapter {
  provider_name: String,
  base_url: String,
  api_key: String,
  client: Client,
}

impl ClaudeAdapter {
  pub fn new(provider_name: String, base_url: String, api_key: String) -> Self {
    Self {
      provider_name,
      base_url,
      api_key,
      client: Client::new(),
    }
  }
}

#[derive(Debug, Serialize)]
struct ClaudeMessageRequest {
  model: String,
  messages: Vec<ClaudeMessage>,
  max_tokens: u32,
  #[serde(skip_serializing_if = "Option::is_none")]
  system: Option<String>,
}

#[derive(Debug, Serialize)]
struct ClaudeMessage {
  role: String,
  content: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeResponse {
  id: String,
  content: Vec<ClaudeContent>,
  stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeContent {
  #[serde(rename = "type")]
  _type: String,
  text: Option<String>,
}

#[async_trait]
impl ProviderAdapter for ClaudeAdapter {
  fn provider_name(&self) -> &str {
    &self.provider_name
  }

  async fn send_chat(
    &self,
    _ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<OpenAiChatCompletionResponse, RouterError> {
    let mut system_chunks = Vec::new();
    let mut messages = Vec::new();
    for m in &req.messages {
      if m.role == "system" {
        system_chunks.push(m.content.clone());
      } else {
        messages.push(ClaudeMessage {
          role: if m.role == "assistant" {
            "assistant".to_string()
          } else {
            "user".to_string()
          },
          content: m.content.clone(),
        });
      }
    }

    let payload = ClaudeMessageRequest {
      model: meta.provider_model.clone(),
      messages,
      max_tokens: 1024,
      system: if system_chunks.is_empty() {
        None
      } else {
        Some(system_chunks.join("\n\n"))
      },
    };

    let resp = self
      .client
      .post(format!("{}/v1/messages", self.base_url.trim_end_matches('/')))
      .header("x-api-key", &self.api_key)
      .header("anthropic-version", "2023-06-01")
      .json(&payload)
      .send()
      .await
      .map_err(|e| RouterError::Upstream(e.to_string()))?;

    if !resp.status().is_success() {
      let body = resp.text().await.unwrap_or_default();
      return Err(RouterError::Upstream(format!(
        "{} messages failed: {}",
        self.provider_name, body
      )));
    }

    let parsed = resp
      .json::<ClaudeResponse>()
      .await
      .map_err(|e| RouterError::Upstream(format!("decode claude response failed: {}", e)))?;

    let content = parsed
      .content
      .iter()
      .filter_map(|c| c.text.clone())
      .collect::<Vec<_>>()
      .join("\n");

    Ok(OpenAiChatCompletionResponse {
      id: parsed.id,
      object: "chat.completion".to_string(),
      created: Utc::now().timestamp(),
      model: req.model.clone(),
      choices: vec![OpenAiChatChoice {
        index: 0,
        message: OpenAiChatMessage {
          role: "assistant".to_string(),
          content,
        },
        finish_reason: parsed.stop_reason,
      }],
      usage: None,
    })
  }

  async fn send_chat_stream(
    &self,
    ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<ChatStream, RouterError> {
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
    _req: &OpenAiEmbeddingRequest,
    _meta: &ProviderRequestMeta,
  ) -> Result<OpenAiEmbeddingResponse, RouterError> {
    Err(RouterError::BadRequest(
      "claude adapter does not support embeddings in v1".to_string(),
    ))
  }

  async fn list_models(&self, _ctx: &RequestContext) -> Result<OpenAiModelList, RouterError> {
    let resp = self
      .client
      .get(format!("{}/v1/models", self.base_url.trim_end_matches('/')))
      .header("x-api-key", &self.api_key)
      .header("anthropic-version", "2023-06-01")
      .send()
      .await
      .map_err(|e| RouterError::Upstream(e.to_string()))?;

    if !resp.status().is_success() {
      return Ok(OpenAiModelList {
        object: "list".to_string(),
        data: vec![OpenAiModelData {
          id: format!("{}-fallback", self.provider_name),
          object: "model".to_string(),
          created: Utc::now().timestamp(),
          owned_by: self.provider_name.clone(),
        }],
      });
    }

    let body = resp
      .json::<serde_json::Value>()
      .await
      .map_err(|e| RouterError::Upstream(format!("decode claude model list failed: {}", e)))?;

    let data = body
      .get("data")
      .and_then(|v| v.as_array())
      .map(|items| {
        items
          .iter()
          .filter_map(|m| {
            Some(OpenAiModelData {
              id: m.get("id")?.as_str()?.to_string(),
              object: "model".to_string(),
              created: Utc::now().timestamp(),
              owned_by: self.provider_name.clone(),
            })
          })
          .collect::<Vec<_>>()
      })
      .unwrap_or_default();

    Ok(OpenAiModelList {
      object: "list".to_string(),
      data,
    })
  }
}
