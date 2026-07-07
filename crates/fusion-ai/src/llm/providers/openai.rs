//! OpenAI 官方 endpoint 实现。
//!
//! 默认 endpoint：`https://api.openai.com/v1`，model `gpt-4o-mini`。
//! `organization` 可选；填了会通过 `OpenAI-Organization` header 下发。

use std::time::Duration;

use async_trait::async_trait;

use crate::llm::wire_openai_compat::OpenAiCompatTransport;
use crate::llm::{ChatCompletionRequest, ChatCompletionResponse, LlmChatProvider, LlmError, LlmProviderId};

pub const DEFAULT_MODEL_OPENAI: &str = "gpt-4o-mini";
pub const DEFAULT_ENDPOINT_OPENAI: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct OpenAiChatProvider {
  transport: OpenAiCompatTransport,
  default_model: String,
}

impl OpenAiChatProvider {
  pub fn new(
    api_key: impl Into<String>,
    organization: Option<String>,
    endpoint: Option<String>,
    default_model: impl Into<String>,
    timeout: Option<Duration>,
  ) -> Result<Self, LlmError> {
    let base_url = endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT_OPENAI.to_string());
    let mut transport = OpenAiCompatTransport::new(base_url, api_key)?;
    if let Some(t) = timeout {
      transport = transport.with_timeout(t);
    }
    if let Some(org) = organization.filter(|s| !s.is_empty()) {
      transport = transport.with_extra_header("OpenAI-Organization", org);
    }
    Ok(Self { transport, default_model: default_model.into() })
  }

  pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
    self.transport = self.transport.with_base_url(base_url);
    self
  }
}

#[async_trait]
impl LlmChatProvider for OpenAiChatProvider {
  fn provider_id(&self) -> LlmProviderId {
    LlmProviderId::OpenAi
  }

  fn default_model(&self) -> &str {
    &self.default_model
  }

  async fn chat_complete(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError> {
    self.transport.chat_complete(LlmProviderId::OpenAi, &self.default_model, req).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn openai_default_model_is_gpt4o_mini() {
    assert_eq!(DEFAULT_MODEL_OPENAI, "gpt-4o-mini");
  }

  #[test]
  fn openai_provider_id_matches() {
    let p = OpenAiChatProvider::new("sk", None, None, DEFAULT_MODEL_OPENAI, None).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::OpenAi);
    assert!(p.transport.base_url().contains("api.openai.com"));
  }
}
