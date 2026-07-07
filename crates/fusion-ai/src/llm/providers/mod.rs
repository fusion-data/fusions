//! [`super::LlmChatProvider`] 实现集合。
//!
//! - [`qwen`] —— 阿里云 DashScope OpenAI 兼容
//! - [`deepseek`] —— DeepSeek 官方 OpenAI 兼容
//! - [`openai`] —— OpenAI 官方
//! - [`anthropic`] —— stub（本期 unimplemented）
//! - [`gemini`] —— stub（本期 unimplemented）

pub mod anthropic;
pub mod deepseek;
pub mod gemini;
pub mod openai;
pub mod qwen;

pub use anthropic::AnthropicChatProvider;
pub use deepseek::DeepSeekChatProvider;
pub use gemini::GeminiChatProvider;
pub use openai::OpenAiChatProvider;
pub use qwen::QwenChatProvider;
