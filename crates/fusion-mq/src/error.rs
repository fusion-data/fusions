use std::time::Duration;
use thiserror::Error;

/// fusion-mq 错误模型。
///
/// 跨 crate `From<MqError> for fusions::DataError` 暂未提供 —— 待该 crate 进入主线
/// `fusions` 聚合时再统一迁入 `fusions::error`（fusions skill 规约）。
/// 当前 hylx 仓内业务 caller 在 service 边界自行 `map_err`。
#[derive(Debug, Error)]
pub enum MqError {
  #[error("mq config invalid: {0}")]
  ConfigInvalid(String),

  #[error("mq connection failed: {0}")]
  ConnectionFailed(String),

  #[error("mq publish failed: {0}")]
  PublishFailed(String),

  #[error("mq claim failed: {0}")]
  ClaimFailed(String),

  #[error("mq ack failed: {0}")]
  AckFailed(String),

  #[error("mq serialization failed: {0}")]
  Serialization(String),

  #[error("mq operation timed out after {0:?}")]
  Timeout(Duration),

  #[error("mq event not found: {0}")]
  NotFound(String),

  #[cfg(feature = "with-postgres")]
  #[error(transparent)]
  Sqlx(#[from] sqlx::Error),

  #[error(transparent)]
  Json(#[from] serde_json::Error),
}
