//! 阿里云 DashScope（Qwen 系列）OpenAI 兼容 endpoint 实现。
//!
//! - 北京：`https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions`
//! - 新加坡：`https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions`
//!
//! `workspace_id` 可选；填了会通过 `X-DashScope-WorkSpace` header 下发。
//! 默认 model `qwen3.7-plus`，caller 可在 [`super::super::ChatCompletionRequest::model`] 覆盖。

use std::time::Duration;

use async_trait::async_trait;

use crate::llm::wire_openai_compat::OpenAiCompatTransport;
use crate::llm::{ChatCompletionRequest, ChatCompletionResponse, LlmChatProvider, LlmError, LlmProviderId};
use crate::providers::dashscope::DashScopeRegion;

pub const DEFAULT_MODEL_QWEN: &str = "qwen3.7-plus";

#[derive(Debug, Clone)]
pub struct QwenChatProvider {
  transport: OpenAiCompatTransport,
  default_model: String,
}

impl QwenChatProvider {
  pub fn new(
    api_key: impl Into<String>,
    workspace_id: Option<String>,
    region: DashScopeRegion,
    default_model: impl Into<String>,
    timeout: Option<Duration>,
  ) -> Result<Self, LlmError> {
    let base_url = match region {
      DashScopeRegion::Beijing => "https://dashscope.aliyuncs.com/compatible-mode/v1",
      DashScopeRegion::Singapore => "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
    };
    let mut transport = OpenAiCompatTransport::new(base_url, api_key)?;
    if let Some(t) = timeout {
      transport = transport.with_timeout(t);
    }
    if let Some(ws) = workspace_id.filter(|s| !s.is_empty()) {
      transport = transport.with_extra_header("X-DashScope-WorkSpace", ws);
    }
    Ok(Self { transport, default_model: default_model.into() })
  }

  /// 测试 / 集成：注入自定义 base_url（如 wiremock）。
  pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
    self.transport = self.transport.with_base_url(base_url);
    self
  }
}

#[async_trait]
impl LlmChatProvider for QwenChatProvider {
  fn provider_id(&self) -> LlmProviderId {
    LlmProviderId::Qwen
  }

  fn default_model(&self) -> &str {
    &self.default_model
  }

  async fn chat_complete(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError> {
    self.transport.chat_complete(LlmProviderId::Qwen, &self.default_model, req).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn qwen_default_model_is_qwen3_max() {
    assert_eq!(DEFAULT_MODEL_QWEN, "qwen3.7-plus");
  }

  #[test]
  fn qwen_provider_id_matches() {
    let p = QwenChatProvider::new("sk", None, DashScopeRegion::Beijing, DEFAULT_MODEL_QWEN, None).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::Qwen);
    assert_eq!(p.default_model(), "qwen3.7-plus");
  }

  #[test]
  fn qwen_singapore_uses_intl_endpoint() {
    let p = QwenChatProvider::new("sk", None, DashScopeRegion::Singapore, DEFAULT_MODEL_QWEN, None).unwrap();
    assert!(p.transport.base_url().contains("dashscope-intl.aliyuncs.com"));
  }
}
