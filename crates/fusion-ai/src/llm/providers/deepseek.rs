//! DeepSeek 官方 OpenAI 兼容 endpoint 实现。
//!
//! 默认 endpoint：`https://api.deepseek.com/v1`，model `deepseek-v4-flash`。
//! 国内反向代理 / 私有化部署可通过 `endpoint` 覆盖。

use std::time::Duration;

use async_trait::async_trait;

use crate::llm::wire_openai_compat::OpenAiCompatTransport;
use crate::llm::{ChatCompletionRequest, ChatCompletionResponse, LlmChatProvider, LlmError, LlmProviderId};

pub const DEFAULT_MODEL_DEEPSEEK: &str = "deepseek-v4-flash";
pub const DEFAULT_ENDPOINT_DEEPSEEK: &str = "https://api.deepseek.com/v1";

#[derive(Debug, Clone)]
pub struct DeepSeekChatProvider {
  transport: OpenAiCompatTransport,
  default_model: String,
}

impl DeepSeekChatProvider {
  pub fn new(
    api_key: impl Into<String>,
    endpoint: Option<String>,
    default_model: impl Into<String>,
    timeout: Option<Duration>,
  ) -> Result<Self, LlmError> {
    let base_url = endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT_DEEPSEEK.to_string());
    let mut transport = OpenAiCompatTransport::new(base_url, api_key)?;
    if let Some(t) = timeout {
      transport = transport.with_timeout(t);
    }
    Ok(Self { transport, default_model: default_model.into() })
  }

  pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
    self.transport = self.transport.with_base_url(base_url);
    self
  }
}

#[async_trait]
impl LlmChatProvider for DeepSeekChatProvider {
  fn provider_id(&self) -> LlmProviderId {
    LlmProviderId::DeepSeek
  }

  fn default_model(&self) -> &str {
    &self.default_model
  }

  async fn chat_complete(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError> {
    self.transport.chat_complete(LlmProviderId::DeepSeek, &self.default_model, req).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn deepseek_default_model_is_deepseek_chat() {
    assert_eq!(DEFAULT_MODEL_DEEPSEEK, "deepseek-v4-flash");
  }

  #[test]
  fn deepseek_provider_id_matches() {
    let p = DeepSeekChatProvider::new("sk", None, DEFAULT_MODEL_DEEPSEEK, None).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::DeepSeek);
    assert!(p.transport.base_url().contains("api.deepseek.com"));
  }

  #[test]
  fn deepseek_custom_endpoint_used() {
    let p =
      DeepSeekChatProvider::new("sk", Some("http://proxy.local/v1".into()), DEFAULT_MODEL_DEEPSEEK, None).unwrap();
    assert_eq!(p.transport.base_url(), "http://proxy.local/v1");
  }
}
