//! Multi-provider LLM chat 抽象 —— 给 hylx-ai voice NLU / 未来 summary / RAG
//! 等 LLM 业务复用。本期解决"单轮 chat + function calling"场景，stream / multi-turn
//! 历史等沿用 fusion-ai 既有的 rig re-export。
//!
//! ## 与现有 [`crate::providers::openai_compatible`] 的关系
//!
//! `providers/openai_compatible/` 给 rig 0.27+ 提供 ProviderClient impl，绑死
//! rig builder DSL。NLU 单轮 function call 场景下 rig builder 抽象过厚（chat
//! history / message thread / completion choice 等），用 reqwest 直调 OpenAI
//! 兼容 endpoint 更直观。两套实现共存：
//! - [`crate::providers`] 模块 → rig agent / chat thread 等 multi-turn 场景
//! - [`crate::llm`] 模块 → 单轮 chat + function call（本模块，可配 trait 加载）
//!
//! ## 分层
//!
//! ```text
//! 业务侧 (hylx-voice NLU)
//!   └── 持 Arc<dyn LlmChatProvider>
//!         └── 通过 [`factory::build_provider`] 构造，按 [`LlmProviderConfig`] 派发
//!               ├── Qwen     (DashScope OpenAI 兼容)  → wire_openai_compat
//!               ├── DeepSeek (api.deepseek.com)       → wire_openai_compat
//!               ├── OpenAi   (api.openai.com)         → wire_openai_compat
//!               ├── Anthropic (stub: unimplemented)
//!               └── Gemini    (stub: unimplemented)
//! ```

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod factory;
pub mod metered;
pub mod providers;
pub mod wire_openai_compat;

pub use factory::{LlmProviderConfig, build_provider};
pub use metered::{AiUsageCtx, AiUsageEvent, AiUsageSink, MatchedScope, MeteredLlmProvider, NoopUsageSink, Outcome};

/// 内置 LLM provider 标识 —— 与 `provider_credentials.provider` 列约定的字符串
/// 一一对应，避免裸字符串 typo。
///
/// # 与 [`crate::DefaultProvider`] 的区别
///
/// `LlmProviderId` 服务 **llm chat 路径**（[`factory::build_provider`] /
/// [`LlmProviderConfig`]），只覆盖本 crate 自研 wire 实装的少数 provider，
/// `as_str()` 返回与 `provider_credentials.provider` 列对齐的约定字符串
/// （注意 Qwen → `"dashscope"`）。[`crate::DefaultProvider`] 服务 **rig factory
/// 路径**（`factory::ClientFactory`），覆盖 19 个 rig 上游 provider，`as_str()`
/// 约定与此不同。两者服务不同子系统、`as_str()` 约定不同，**不要混用**。
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LlmProviderId {
  /// 阿里云 DashScope OpenAI 兼容（Qwen 系列）—— `provider='dashscope'`
  Qwen,
  /// DeepSeek 官方 OpenAI 兼容 —— `provider='deepseek'`
  DeepSeek,
  /// OpenAI 官方 —— `provider='openai'`
  OpenAi,
  /// Anthropic Claude —— `provider='anthropic'`（本期 stub）
  Anthropic,
  /// Google Gemini —— `provider='gemini'`（本期 stub）
  Gemini,
}

impl LlmProviderId {
  /// 稳定字符串标识（与 `provider_credentials.provider` 列对齐）。
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Qwen => "dashscope",
      Self::DeepSeek => "deepseek",
      Self::OpenAi => "openai",
      Self::Anthropic => "anthropic",
      Self::Gemini => "gemini",
    }
  }

  pub fn from_str_ci(s: &str) -> Option<Self> {
    match s.to_ascii_lowercase().as_str() {
      "dashscope" | "qwen" => Some(Self::Qwen),
      "deepseek" => Some(Self::DeepSeek),
      "openai" => Some(Self::OpenAi),
      "anthropic" => Some(Self::Anthropic),
      "gemini" => Some(Self::Gemini),
      _ => None,
    }
  }
}

impl std::fmt::Display for LlmProviderId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Chat role —— OpenAI 兼容字符串（`system` / `user` / `assistant` / `tool`）。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ChatRole {
  System,
  User,
  Assistant,
  Tool,
}

impl ChatRole {
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::System => "system",
      Self::User => "user",
      Self::Assistant => "assistant",
      Self::Tool => "tool",
    }
  }
}

/// 单条 chat message。`tool_calls` 为模型主动调函数时的输出（assistant role）。
#[derive(Debug, Clone)]
pub struct ChatMessage {
  pub role: ChatRole,
  /// `content` 可为空字符串（function call 的 assistant message 通常 content 为空）
  pub content: String,
  /// 模型生成的 function call 列表（OpenAI 兼容 `tool_calls`），仅 assistant role 携带
  pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
  pub fn system(content: impl Into<String>) -> Self {
    Self { role: ChatRole::System, content: content.into(), tool_calls: Vec::new() }
  }
  pub fn user(content: impl Into<String>) -> Self {
    Self { role: ChatRole::User, content: content.into(), tool_calls: Vec::new() }
  }
}

/// 函数调用定义 —— OpenAI 兼容 `tools[i]`（type=function）。
#[derive(Debug, Clone)]
pub struct ToolDefinition {
  pub name: String,
  pub description: Option<String>,
  /// JSON Schema 描述 function 参数；caller 自构造，trait 不感知字段
  pub parameters: serde_json::Value,
}

/// 模型生成的 function call。
#[derive(Debug, Clone)]
pub struct ToolCall {
  /// vendor 分配的 id（OpenAI / Qwen 都返回）；可空兜底
  pub id: Option<String>,
  pub name: String,
  /// `arguments` 是 JSON 字符串（vendor 约定，非已解析对象）
  pub arguments: String,
}

/// 强制 tool 选择策略。
#[derive(Debug, Clone)]
pub enum ToolChoice {
  /// 让模型自由选择 — 等价于不显式传 `tool_choice`
  Auto,
  /// 必须调函数 — 等价于 `tool_choice="required"`
  Required,
  /// 必须调指定函数 — `tool_choice={"type":"function","function":{"name":"..."}}`
  Function(String),
}

#[derive(Debug, Clone, Default)]
pub struct ChatCompletionRequest {
  /// None → 取 [`LlmChatProvider::default_model`]
  pub model: Option<String>,
  pub system_prompt: Option<String>,
  pub messages: Vec<ChatMessage>,
  pub tools: Vec<ToolDefinition>,
  pub tool_choice: Option<ToolChoice>,
  /// None = 不下发字段（用 vendor 默认）
  pub temperature: Option<f32>,
  /// None = 用 transport 默认 timeout；Some = 单请求覆盖（wire 层经
  /// `RequestBuilder::timeout` 下发，覆盖 client 默认值）
  pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
  pub prompt_tokens: u32,
  pub completion_tokens: u32,
  pub total_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct ChatCompletionResponse {
  pub model: String,
  pub message: ChatMessage,
  pub usage: Option<TokenUsage>,
  /// 透传 vendor-specific 元数据（如 request_id / quotas / finish_reason）；
  /// hylx-voice NLU 会把这块塞进 `voice_drafts.provider_metadata` 列做审计。
  pub provider_metadata: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
  #[error("provider {0} not enabled (stub / feature flag off)")]
  ProviderNotEnabled(LlmProviderId),

  #[error("provider {0} config invalid: {1}")]
  ConfigInvalid(LlmProviderId, String),

  #[error("transport error talking to {provider}: {message}")]
  Transport { provider: LlmProviderId, message: String },

  #[error("upstream {provider} returned HTTP {status}: {message}")]
  Http { provider: LlmProviderId, status: u16, message: String },

  #[error("upstream {provider} response parse error: {message}")]
  ResponseParse { provider: LlmProviderId, message: String },

  #[error("upstream {provider} returned no message in choices")]
  NoChoice { provider: LlmProviderId },
}

impl LlmError {
  /// 简单分类：true = 可重试瞬态（5xx / timeout / transport），false = 永久错误
  /// （4xx / parse / config）。trait 实现可用此判断是否回退 AMBIGUOUS / 重试。
  pub fn is_retryable(&self) -> bool {
    match self {
      Self::Transport { .. } => true,
      Self::Http { status, .. } => *status >= 500,
      Self::ResponseParse { .. } | Self::NoChoice { .. } | Self::ConfigInvalid(..) | Self::ProviderNotEnabled(..) => {
        false
      }
    }
  }
}

/// LLM chat provider trait —— 单轮 chat completion + function calling。
///
/// 实现者保证：
/// - `chat_complete` 是 cancel-safe（caller drop 时 reqwest 会取消 in-flight 请求）
/// - 错误返回前 **不要**写日志（caller 负责，trait 只透传）
/// - `provider_metadata` 字段不要 leak plaintext secrets / api_key
#[async_trait]
pub trait LlmChatProvider: Send + Sync {
  fn provider_id(&self) -> LlmProviderId;

  /// `model` 字段缺省时 caller 使用的回退值。
  fn default_model(&self) -> &str;

  async fn chat_complete(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError>;
}

/// Boxed alias —— factory 输出 + caller 持有的统一类型。
pub type SharedLlmChatProvider = Arc<dyn LlmChatProvider>;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn provider_id_string_round_trip() {
    for p in [
      LlmProviderId::Qwen,
      LlmProviderId::DeepSeek,
      LlmProviderId::OpenAi,
      LlmProviderId::Anthropic,
      LlmProviderId::Gemini,
    ] {
      assert_eq!(LlmProviderId::from_str_ci(p.as_str()), Some(p));
    }
  }

  #[test]
  fn provider_id_accepts_qwen_alias() {
    assert_eq!(LlmProviderId::from_str_ci("qwen"), Some(LlmProviderId::Qwen));
    assert_eq!(LlmProviderId::from_str_ci("QWEN"), Some(LlmProviderId::Qwen));
  }

  #[test]
  fn provider_id_unknown_returns_none() {
    assert_eq!(LlmProviderId::from_str_ci("cohere"), None);
  }

  #[test]
  fn error_retryable_classifies_correctly() {
    let transport = LlmError::Transport { provider: LlmProviderId::Qwen, message: "tcp reset".into() };
    let http_5xx = LlmError::Http { provider: LlmProviderId::Qwen, status: 503, message: "down".into() };
    let http_4xx = LlmError::Http { provider: LlmProviderId::Qwen, status: 401, message: "bad key".into() };
    let parse = LlmError::ResponseParse { provider: LlmProviderId::Qwen, message: "bad json".into() };
    let no_enable = LlmError::ProviderNotEnabled(LlmProviderId::Anthropic);
    assert!(transport.is_retryable());
    assert!(http_5xx.is_retryable());
    assert!(!http_4xx.is_retryable());
    assert!(!parse.is_retryable());
    assert!(!no_enable.is_retryable());
  }
}
