use std::{ops::Deref, sync::Arc, time::Duration};

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::time::now_offset;

#[derive(Debug, Error)]
pub enum CtxError {
  #[error("Invalid payload, need a json object")]
  InvalidPayload,

  #[error("Unauthorized: {0}")]
  Unauthorized(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
#[serde(transparent)]
pub struct CtxPayload(Map<String, Value>);

impl CtxPayload {
  pub fn new(payload: Map<String, Value>) -> Self {
    Self(payload)
  }

  pub fn into_inner(self) -> Map<String, Value> {
    self.0
  }

  pub fn set_subject(&mut self, value: impl Into<String>) {
    self.set_string(Ctx::SUB, value);
  }

  pub fn set_tenant_id(&mut self, value: impl Into<String>) {
    self.set_string(Ctx::TENANT_ID, value);
  }

  /// Set the expiration time with `DateTime<Utc>`.
  pub fn set_expires_at(&mut self, value: impl Into<DateTime<Utc>>) {
    let utc: DateTime<Utc> = value.into();
    self.set_exp(utc.timestamp());
  }

  /// Set the expiration time in seconds since the Unix epoch.
  pub fn set_exp(&mut self, epoch_seconds: i64) {
    self.set_i64(Ctx::EXP, epoch_seconds);
  }

  pub fn set_string(&mut self, key: &str, value: impl Into<String>) {
    self.0.insert(key.to_string(), Value::String(value.into()));
  }

  pub fn set_i64(&mut self, key: &str, value: i64) {
    self.0.insert(key.to_string(), Value::Number(value.into()));
  }

  pub fn set_i32(&mut self, key: &str, value: i32) {
    self.0.insert(key.to_string(), Value::Number(value.into()));
  }

  pub fn set_datetime(&mut self, key: &str, value: impl Into<DateTime<Utc>>) {
    let utc: DateTime<Utc> = value.into();
    self.0.insert(key.to_string(), Value::Number(utc.timestamp().into()));
  }

  pub fn set_bool(&mut self, key: &str, value: bool) {
    self.0.insert(key.to_string(), Value::Bool(value));
  }

  pub fn set_strings<I, S>(&mut self, key: &str, value: I)
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    self
      .0
      .insert(key.to_string(), Value::Array(value.into_iter().map(|s| Value::String(s.into())).collect()));
  }

  pub fn get_subject(&self) -> Option<&str> {
    self.get_str(Ctx::SUB)
  }

  pub fn get_expires_at(&self) -> Option<DateTime<Utc>> {
    self.get_exp().and_then(DateTime::<Utc>::from_timestamp_secs)
  }

  pub fn get_exp(&self) -> Option<i64> {
    self.get_i64(Ctx::EXP)
  }

  pub fn get_str(&self, key: &str) -> Option<&str> {
    self.0.get(key).and_then(|s| s.as_str())
  }

  pub fn get_i64(&self, key: &str) -> Option<i64> {
    self.0.get(key).and_then(|s| s.as_i64())
  }

  pub fn get_i32(&self, key: &str) -> Option<i32> {
    self.0.get(key).and_then(|s| s.as_i64()).map(|v| v as i32)
  }

  pub fn get_strings(&self, key: &str) -> Option<Vec<&str>> {
    self.0.get(key).and_then(|s| s.as_array()).map(|v| v.iter().filter_map(|s| s.as_str()).collect())
  }

  pub fn get_bool(&self, key: &str) -> Option<bool> {
    self.0.get(key).and_then(|s| s.as_bool())
  }
}

impl From<Map<String, Value>> for CtxPayload {
  fn from(payload: Map<String, Value>) -> Self {
    Self::new(payload)
  }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
pub struct CtxInner {
  payload: CtxPayload,
  req_time: DateTime<FixedOffset>,
  req_id: String,
}

/// 会话上下文。此处 clone 的成本很低
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
#[serde(transparent)]
pub struct Ctx(Arc<CtxInner>);

impl Ctx {
  /// 常用于保存用户 ID
  pub const SUB: &str = "sub";
  /// 常用于保存用户过期时间，表示的是自 Unix 纪元（1970-01-01T00:00:00Z UTC）以来的秒数
  pub const EXP: &str = "exp";
  /// 租户 ID
  pub const TENANT_ID: &str = "tenant_id";

  pub(crate) fn new(payload: CtxPayload, req_time: DateTime<FixedOffset>, req_id: String) -> Self {
    Self(Arc::new(CtxInner { payload, req_time, req_id }))
  }

  /// Create a new context
  pub fn try_new(
    mut payload: CtxPayload,
    req_time: Option<DateTime<FixedOffset>>,
    req_id: Option<String>,
  ) -> Result<Self, CtxError> {
    let now = now_offset();
    if let Some(st) = payload.get_expires_at() {
      if st < now {
        return Err(CtxError::Unauthorized("The token expired".to_string()));
      }
    } else {
      let exp = now + Duration::from_secs(60 * 60 * 24 * 60); // 60 days
      payload.set_expires_at(exp.to_utc());
    }

    Ok(Self::new(payload, req_time.unwrap_or(now), req_id.unwrap_or_default()))
  }

  pub fn new_root() -> Self {
    let req_time = now_offset();
    let expires_at = req_time + Duration::from_secs(60 * 30); // 30 minutes
    let mut payload = CtxPayload::default();
    payload.set_string(Self::SUB, "0");
    payload.set_expires_at(expires_at);
    Self::new(payload, req_time, "".to_string())
  }

  pub fn new_super_admin() -> Self {
    let req_time = now_offset();
    let expires_at = req_time + Duration::from_secs(60 * 30); // 30 minutes
    let mut payload = CtxPayload::default();
    payload.set_string(Self::SUB, "1");
    payload.set_expires_at(expires_at);
    Self::new(payload, req_time, "".to_string())
  }

  pub fn get_user_id(&self) -> Option<i64> {
    self.payload.get_subject().and_then(|s| s.parse().ok())
  }

  /// **Deprecated**: 返回 `i64`，缺失时返 `0`，与 root user_id（如 PG `app.user_id=0`
  /// 表示 system maintenance）撞车 — caller 看到 0 无法区分"匿名"与"系统"。
  /// 业务代码请用 [`Self::get_user_id`]（返 `Option<i64>`，缺失时 `None`）；
  /// 仍需 fallback 默认值的旧代码改写 `ctx.get_user_id().unwrap_or(0)`，让 0
  /// 语义在调用点显式可见。
  #[deprecated(
    note = "Returns 0 when sub claim is missing/non-numeric, conflating anonymous with root user_id=0. Prefer get_user_id() -> Option<i64> and handle None at the call site."
  )]
  pub fn user_id(&self) -> i64 {
    self.get_user_id().unwrap_or(0)
  }

  /// Get tenant ID as `&str`. Unlike `user_id()` (which returns `i64` parsed from `sub`),
  /// this returns the raw string value — tenant IDs are UUIDs, not integers.
  pub fn get_tenant_id(&self) -> Option<&str> {
    self.payload.get_str(Self::TENANT_ID)
  }

  /// Convenience accessor. Returns `""` when tenant_id is not set in the payload.
  /// Callers that require a tenant_id should use `get_tenant_id()` and handle the `None` case.
  pub fn tenant_id(&self) -> &str {
    self.get_tenant_id().unwrap_or_default()
  }

  pub fn req_time(&self) -> &DateTime<FixedOffset> {
    &self.req_time
  }

  pub fn req_epoch_secs(&self) -> i64 {
    self.req_time.timestamp()
  }

  pub fn req_epoch_millis(&self) -> i64 {
    self.req_time.timestamp_millis()
  }

  pub fn req_id(&self) -> &str {
    &self.req_id
  }

  pub fn expires_at(&self) -> Option<DateTime<Utc>> {
    self.payload.get_expires_at()
  }

  pub fn payload(&self) -> &CtxPayload {
    &self.payload
  }
}

impl Deref for Ctx {
  type Target = CtxInner;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}
