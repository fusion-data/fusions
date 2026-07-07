use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// 全局事件 ID（UUID v7，由 DB 默认值生成）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub Uuid);

impl EventId {
  pub fn new(uuid: Uuid) -> Self {
    Self(uuid)
  }

  pub fn into_uuid(self) -> Uuid {
    self.0
  }
}

impl fmt::Display for EventId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.0)
  }
}

impl From<Uuid> for EventId {
  fn from(value: Uuid) -> Self {
    Self(value)
  }
}

impl From<EventId> for Uuid {
  fn from(value: EventId) -> Self {
    value.0
  }
}

/// 事件状态机：`pending` → `processing` → `completed | failed`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventStatus {
  Pending,
  Processing,
  Completed,
  Failed,
}

impl EventStatus {
  pub fn as_str(&self) -> &'static str {
    match self {
      EventStatus::Pending => "pending",
      EventStatus::Processing => "processing",
      EventStatus::Completed => "completed",
      EventStatus::Failed => "failed",
    }
  }
}

impl FromStr for EventStatus {
  type Err = String;

  /// 与 [`EventStatus::as_str`] 对称的反向转换。
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "pending" => Ok(EventStatus::Pending),
      "processing" => Ok(EventStatus::Processing),
      "completed" => Ok(EventStatus::Completed),
      "failed" => Ok(EventStatus::Failed),
      other => Err(format!("invalid EventStatus: `{}`", other)),
    }
  }
}

/// 待发布事件。
///
/// `payload` 内嵌业务上下文（tenant_id 等）由 caller 负责；本 crate 不解析。
#[derive(Debug, Clone)]
pub struct PublishEvent {
  pub event_type: String,
  pub source_service: String,
  pub target_service: String,
  pub payload: serde_json::Value,
  /// 重试上限；`None` 时 provider 走默认值（Postgres provider = 3）。
  pub max_retries: Option<u32>,
}

impl PublishEvent {
  pub fn new(
    event_type: impl Into<String>,
    source_service: impl Into<String>,
    target_service: impl Into<String>,
    payload: serde_json::Value,
  ) -> Self {
    Self {
      event_type: event_type.into(),
      source_service: source_service.into(),
      target_service: target_service.into(),
      payload,
      max_retries: None,
    }
  }

  pub fn with_max_retries(mut self, max_retries: u32) -> Self {
    self.max_retries = Some(max_retries);
    self
  }
}

/// 已 claim 的事件（status = processing）。
#[derive(Debug, Clone)]
pub struct ClaimedEvent {
  pub id: EventId,
  pub event_type: String,
  pub payload: serde_json::Value,
  pub retry_count: u32,
  pub max_retries: u32,
  pub created_at: DateTime<Utc>,
}
