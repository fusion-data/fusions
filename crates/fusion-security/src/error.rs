//! fusion-security 模块的统一错误类型。

use thiserror::Error;

pub type SecurityResult<T> = core::result::Result<T, SecurityError>;

#[derive(Debug, Error)]
pub enum SecurityError {
  #[error("Failed to generate token")]
  TokenGeneration,

  #[error("Failed to verify token: {0}")]
  TokenVerification(String),

  #[error("Token expired")]
  TokenExpired,

  #[error("Invalid token format")]
  InvalidToken,

  #[error("OAuth error: {0}")]
  OAuth(String),

  #[error(transparent)]
  Core(#[from] fusion_core::security::Error),

  #[error("{0}")]
  Custom(String),
}
