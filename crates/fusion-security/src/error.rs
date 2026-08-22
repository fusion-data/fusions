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

  #[error("Failed to hash password")]
  FailedToHashPassword,

  #[error("Invalid password")]
  InvalidPassword,

  #[error("Failed to verify password")]
  FailedToVerifyPassword,

  /// 存储的密码哈希串不符合 PHC 格式（`PasswordHash::new` 解析失败）。
  #[error("Invalid password hash format")]
  InvalidHashFormat,

  /// `tokio::task::spawn_blocking` JoinError —— argon2 worker 线程异常
  /// （runtime shutdown / OOM / panic）。生产路径应 graceful 上报而非 panic。
  #[error("Password worker join failed: {0}")]
  PasswordWorkerJoinFailed(String),

  #[error(transparent)]
  Core(#[from] fusion_core::security::Error),

  #[error("{0}")]
  Custom(String),
}
