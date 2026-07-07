//! Anthropic Claude provider —— 本期 stub。
//!
//! Anthropic 的 message API（`/v1/messages`）与 OpenAI 兼容 endpoint 不兼容
//! （body shape / tool_use 协议差异），需要专用 wire 层。本期 trait + factory
//! 已展示在前端 `ListProviders` 中，租户配置后切换路由到 anthropic 会在
//! [`LlmChatProvider::chat_complete`] 返回 [`LlmError::ProviderNotEnabled`]，
//! hylx-voice ws handshake 阶段就拒绝并报错给前端。
//!
//! 实装触发条件（与 fusion-ai/llm 设计文档对齐）：
//! 1. 至少 1 家试点客户明确要求 Anthropic Claude
//! 2. 或 OpenAI 海外通道达成不了的 governance 场景
//!
//! 在以上触发之前不写 wire 实现。

use async_trait::async_trait;

use crate::llm::{ChatCompletionRequest, ChatCompletionResponse, LlmChatProvider, LlmError, LlmProviderId};

pub const DEFAULT_MODEL_ANTHROPIC: &str = "claude-3-5-sonnet-latest";

#[derive(Debug, Clone)]
pub struct AnthropicChatProvider {
  default_model: String,
}

impl AnthropicChatProvider {
  pub fn new(_api_key: impl Into<String>, default_model: impl Into<String>) -> Self {
    Self { default_model: default_model.into() }
  }
}

#[async_trait]
impl LlmChatProvider for AnthropicChatProvider {
  fn provider_id(&self) -> LlmProviderId {
    LlmProviderId::Anthropic
  }

  fn default_model(&self) -> &str {
    &self.default_model
  }

  async fn chat_complete(&self, _req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError> {
    Err(LlmError::ProviderNotEnabled(LlmProviderId::Anthropic))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn anthropic_chat_complete_returns_not_enabled() {
    let p = AnthropicChatProvider::new("sk", DEFAULT_MODEL_ANTHROPIC);
    let err = p.chat_complete(ChatCompletionRequest::default()).await.expect_err("stub must fail");
    assert!(matches!(err, LlmError::ProviderNotEnabled(LlmProviderId::Anthropic)));
  }
}
