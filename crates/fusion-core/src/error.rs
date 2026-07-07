//! fusion-core 统一错误类型。
//!
//! 聚合了 `component`、`configuration`、`security` 等子模块的错误，并兜底
//! 转换 tokio / std::io 等运行期错误。所有 `fusion_core::Result<T>` 均以
//! [`CoreError`] 为错误类型。

use thiserror::Error;

use crate::component::ComponentError;
use crate::configuration::ConfigureError;
use crate::security::Error as SecurityError;

pub type CoreResult<T> = core::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
  #[error(transparent)]
  Component(#[from] ComponentError),

  #[error(transparent)]
  Configure(#[from] ConfigureError),

  #[error(transparent)]
  Security(#[from] SecurityError),

  #[error(transparent)]
  Io(#[from] std::io::Error),

  #[error(transparent)]
  TaskJoin(#[from] tokio::task::JoinError),

  #[error("Tracing init error: {0}")]
  Tracing(String),

  #[error("Timer error: {0}")]
  Timer(String),

  #[error("{0}")]
  Custom(String),
}

impl CoreError {
  pub fn timer(msg: impl Into<String>) -> Self {
    Self::Timer(msg.into())
  }

  pub fn tracing(msg: impl Into<String>) -> Self {
    Self::Tracing(msg.into())
  }

  pub fn custom(msg: impl Into<String>) -> Self {
    Self::Custom(msg.into())
  }
}
