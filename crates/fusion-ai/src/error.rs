//! # Fusion AI Error Types
//!
//! 错误收敛形态（fusion-ai-de-rig.md §4.3）：上游 wire 层错误统一走
//! [`OpenAiCompatError`]，其 `is_upstream_transient()` 承载
//! 「上游瞬态 vs 本地缺陷」分级语义。

use crate::providers::openai_compatible::errors::OpenAiCompatError;

/// AI-related errors
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AiError {
  #[error("Custom error: {0}")]
  Custom(String),

  #[error(transparent)]
  OpenAiCompat(#[from] OpenAiCompatError),
}

impl AiError {
  /// 上游瞬态判定（委托 [`OpenAiCompatError::is_upstream_transient`]）。
  pub fn is_upstream_transient(&self) -> bool {
    match self {
      Self::Custom(_) => false,
      Self::OpenAiCompat(err) => err.is_upstream_transient(),
    }
  }
}
