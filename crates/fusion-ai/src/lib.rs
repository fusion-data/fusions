pub mod agents;
pub mod client;
pub mod embeddings;
mod error;
pub mod graph_flow;
pub mod json_utils;
pub mod llm;
pub mod providers;
pub mod speech_to_text;
pub mod utils;
#[cfg(feature = "video")]
pub mod video_generation;

/// Re-export rig for easier access to core types.
///
/// Note: `rig` is an upstream dependency passed through verbatim. Its
/// version and API are **not** covered by this crate's SemVer guarantees —
/// downstream code that depends directly on `rig` types takes on the upgrade
/// risk itself.
pub use rig;

pub use error::*;

/// New factory module (rig 0.27+ explicit provider pattern)
pub mod factory {
  pub use super::client::{AgentConfig, ClientFactory, EmbeddingConfig, FactoryError};
}

/// 19 个内置 LLM provider 名 — 类型安全 enum，避免裸字符串 `"anthopic"` typo
/// 编译通过运行时 lookup `None` 的 footgun。每个 variant 的 `as_str()` 返回与
/// rig 上游约定的标准短名，可直接喂给 `ClientFactory` 等 lookup 路径。
///
/// # 与 [`llm::LlmProviderId`] 的区别
///
/// `DefaultProvider` 服务 **rig factory 路径**（`factory::ClientFactory` /
/// `client::AgentConfig`），覆盖 19 个 rig 上游 provider，`as_str()` 返回 rig
/// 约定的短名（如 `"openai-compatible"`）。
/// [`llm::LlmProviderId`] 服务 **llm chat 路径**（`llm::factory::build_provider`
/// / `LlmProviderConfig`），只覆盖本 crate 自研 wire 实装的少数 provider，其
/// `as_str()` 约定与 `DefaultProvider` 不同（如 Qwen 返回 `"dashscope"`）。
/// 两者服务不同子系统、`as_str()` 约定不同，**不要混用**。
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum DefaultProvider {
  Anthropic,
  Cohere,
  Gemini,
  HuggingFace,
  OpenAi,
  OpenAiCompatible,
  OpenRouter,
  Together,
  XAi,
  Azure,
  DeepSeek,
  Galadriel,
  Groq,
  Hyperbolic,
  Moonshot,
  Mira,
  Mistral,
  Ollama,
  Perplexity,
}

impl DefaultProvider {
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Anthropic => "anthropic",
      Self::Cohere => "cohere",
      Self::Gemini => "gemini",
      Self::HuggingFace => "huggingface",
      Self::OpenAi => "openai",
      Self::OpenAiCompatible => "openai-compatible",
      Self::OpenRouter => "openrouter",
      Self::Together => "together",
      Self::XAi => "xai",
      Self::Azure => "azure",
      Self::DeepSeek => "deepseek",
      Self::Galadriel => "galadriel",
      Self::Groq => "groq",
      Self::Hyperbolic => "hyperbolic",
      Self::Moonshot => "moonshot",
      Self::Mira => "mira",
      Self::Mistral => "mistral",
      Self::Ollama => "ollama",
      Self::Perplexity => "perplexity",
    }
  }
}

impl AsRef<str> for DefaultProvider {
  fn as_ref(&self) -> &str {
    self.as_str()
  }
}

impl std::fmt::Display for DefaultProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}
