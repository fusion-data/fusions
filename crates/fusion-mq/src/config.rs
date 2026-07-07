use serde::{Deserialize, Serialize};
use std::time::Duration;

/// MQ provider 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MqProviderKind {
  #[default]
  Postgres,
}

/// `[fusion.mq]` 配置段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MqConfig {
  /// 总开关。`false` 时 [`crate::MessageQueuePlugin`] 跳过 provider 构造。
  pub enable: bool,
  pub provider: MqProviderKind,
  pub postgres: PostgresMqConfig,
}

impl Default for MqConfig {
  fn default() -> Self {
    Self { enable: false, provider: MqProviderKind::Postgres, postgres: PostgresMqConfig::default() }
  }
}

/// `[fusion.mq.postgres]` 配置段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PostgresMqConfig {
  /// PostgreSQL 连接 URL。`None` 时 plugin 启动报错。
  pub url: Option<String>,
  pub max_connections: u32,
  pub min_connections: u32,
  pub acquire_timeout_seconds: u64,
  pub idle_timeout_seconds: u64,
  /// `event_queue` 表名（dev/test 隔离用）。仅允许 `[a-zA-Z0-9_]`。
  pub table_name: String,
}

impl Default for PostgresMqConfig {
  fn default() -> Self {
    Self {
      url: None,
      max_connections: 10,
      min_connections: 1,
      acquire_timeout_seconds: 10,
      idle_timeout_seconds: 600,
      table_name: "event_queue".to_string(),
    }
  }
}

impl PostgresMqConfig {
  pub fn acquire_timeout(&self) -> Duration {
    Duration::from_secs(self.acquire_timeout_seconds)
  }

  pub fn idle_timeout(&self) -> Duration {
    Duration::from_secs(self.idle_timeout_seconds)
  }
}
