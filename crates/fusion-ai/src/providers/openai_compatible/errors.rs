//! openai_compatible 错误模型（fusion-ai-de-rig.md §4.3）。
//!
//! 分级语义：`is_upstream_transient()` 为 true 的错误是上游瞬态（provider 5xx /
//! 429 / 连接层失败），消费方应映射为 503 可重试；其余是本地缺陷（请求构造 /
//! 响应解析），重试无意义。

/// OpenAI 兼容 wire 层错误。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenAiCompatError {
  /// provider 返回非 2xx
  #[error("upstream HTTP error ({status}): {message}")]
  Http { status: u16, message: String },

  /// 连接层失败（reqwest send 失败：DNS / 连接拒绝 / 超时）
  #[error("transport error: {0}")]
  Transport(String),

  /// 反序列化 / SSE 帧非法
  #[error("response parse error: {0}")]
  ResponseParse(String),

  /// 请求构造失败
  #[error("request build error: {0}")]
  RequestBuild(String),

  /// 流中途错误
  #[error("stream error: {0}")]
  Stream(String),
}

impl OpenAiCompatError {
  /// 上游瞬态判定：Transport / Http(5xx / 429) → true。
  ///
  /// 消费方据此做「503 可重试 vs 500 本地缺陷」分级（framework-conventions §1）。
  pub fn is_upstream_transient(&self) -> bool {
    match self {
      Self::Transport(_) => true,
      Self::Http { status, .. } => *status == 429 || *status >= 500,
      _ => false,
    }
  }

  pub(crate) fn request_build(msg: impl Into<String>) -> Self {
    Self::RequestBuild(msg.into())
  }
}

impl From<serde_json::Error> for OpenAiCompatError {
  fn from(err: serde_json::Error) -> Self {
    Self::ResponseParse(err.to_string())
  }
}

impl From<reqwest::Error> for OpenAiCompatError {
  fn from(err: reqwest::Error) -> Self {
    Self::Transport(err.to_string())
  }
}

impl From<crate::providers::openai_compatible::types::MessageError> for OpenAiCompatError {
  fn from(err: crate::providers::openai_compatible::types::MessageError) -> Self {
    Self::RequestBuild(err.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn upstream_transient_classification() {
    // 上游瞬态：连接层、5xx、429
    assert!(OpenAiCompatError::Transport("connect refused".into()).is_upstream_transient());
    assert!(OpenAiCompatError::Http { status: 500, message: String::new() }.is_upstream_transient());
    assert!(OpenAiCompatError::Http { status: 503, message: String::new() }.is_upstream_transient());
    assert!(OpenAiCompatError::Http { status: 429, message: String::new() }.is_upstream_transient());

    // 本地缺陷 / 客户端错误：不可按瞬态重试
    assert!(!OpenAiCompatError::Http { status: 400, message: String::new() }.is_upstream_transient());
    assert!(!OpenAiCompatError::Http { status: 401, message: String::new() }.is_upstream_transient());
    assert!(!OpenAiCompatError::ResponseParse("bad json".into()).is_upstream_transient());
    assert!(!OpenAiCompatError::RequestBuild("missing field".into()).is_upstream_transient());
    assert!(!OpenAiCompatError::Stream("mid-stream".into()).is_upstream_transient());
  }
}
