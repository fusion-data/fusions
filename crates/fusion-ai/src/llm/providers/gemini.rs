//! Google Gemini provider —— 本期 stub。
//!
//! Gemini 的 `generateContent` API 与 OpenAI 兼容 endpoint body shape 差异较大
//! （`contents[]` + `system_instruction` + `function_declarations`），需要专用
//! wire 层。本期同 Anthropic stub 处理，切换到 gemini 会触发
//! [`LlmError::ProviderNotEnabled`]。

use async_trait::async_trait;

use crate::llm::{ChatCompletionRequest, ChatCompletionResponse, LlmChatProvider, LlmError, LlmProviderId};

pub const DEFAULT_MODEL_GEMINI: &str = "gemini-2.0-flash";

#[derive(Debug, Clone)]
pub struct GeminiChatProvider {
  default_model: String,
}

impl GeminiChatProvider {
  pub fn new(_api_key: impl Into<String>, default_model: impl Into<String>) -> Self {
    Self { default_model: default_model.into() }
  }
}

#[async_trait]
impl LlmChatProvider for GeminiChatProvider {
  fn provider_id(&self) -> LlmProviderId {
    LlmProviderId::Gemini
  }

  fn default_model(&self) -> &str {
    &self.default_model
  }

  async fn chat_complete(&self, _req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError> {
    Err(LlmError::ProviderNotEnabled(LlmProviderId::Gemini))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn gemini_chat_complete_returns_not_enabled() {
    let p = GeminiChatProvider::new("sk", DEFAULT_MODEL_GEMINI);
    let err = p.chat_complete(ChatCompletionRequest::default()).await.expect_err("stub must fail");
    assert!(matches!(err, LlmError::ProviderNotEnabled(LlmProviderId::Gemini)));
  }
}
