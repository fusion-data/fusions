//! 聚合错误模型 [`DataError`] —— fusion 生态的统一业务错误类型。
//!
//! `DataError` 与各 `fusion-xxx` 子库自有的错误类型（如 `fusion_core::CoreError`、
//! `fusion_security::SecurityError`、`fusion_web::WebError`、`fusion_sql::SqlError`
//! 等）通过本模块的 `From` 实现连接。具体取舍：
//!
//! - 各 `fusion-xxx` crate 仅暴露自有错误类型，不再直接依赖 `DataError`；
//! - `fusions` 作为聚合层集中提供错误转换，业务代码默认 `use fusions::DataError;`
//!   即可拿到统一的错误模型与跨 crate 的 `From` 转换。
//!
//! 错误码遵循 SPECIFICATION.md：`namespace.error_name`（snake_case）。

use std::borrow::Cow;

use fusion_common::ctx::CtxError;
use fusion_common::{Error as CommonError, codes};
use serde::Serialize;

pub type Result<T> = core::result::Result<T, DataError>;
pub type DataResult<T> = core::result::Result<T, DataError>;

/// 业务错误模型 —— HTTP / RPC 友好的统一错误类型。
///
/// 字段遵循 SPECIFICATION.md 错误响应体规范：
/// - `code`：字符串，格式 `namespace.error_name`（snake_case）
/// - `message`：可选，面向调试但不得包含敏感明文
/// - `request_id`：可选，请求追踪 ID
/// - `details`：可选，附加错误详情
///
/// 示例：
/// ```json
/// {
///   "code": "auth.permission_denied",
///   "message": "权限不足",
///   "request_id": "req_abc123",
///   "details": {}
/// }
/// ```
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DataError {
  /// 错误码，格式：`namespace.error_name`（snake_case）
  pub code: Cow<'static, str>,
  /// 可选，错误消息，面向调试但不得包含敏感明文
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<Cow<'static, str>>,
  /// 可选，请求追踪 ID
  #[serde(skip_serializing_if = "Option::is_none")]
  pub request_id: Option<String>,
  /// 可选，附加错误详情（Box 包装减小结构体大小）
  #[serde(skip_serializing_if = "Option::is_none")]
  pub details: Option<Box<serde_json::Value>>,
  /// 源错误（不序列化，仅用于内部追踪）
  #[serde(skip)]
  pub source: Option<Box<dyn core::error::Error + Send + Sync>>,
}

impl core::error::Error for DataError {
  fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
    self.source.as_ref().map(|e| &**e as &(dyn core::error::Error + 'static))
  }
}

impl core::fmt::Display for DataError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.message {
      Some(msg) => write!(f, "{}: {}", self.code, msg),
      None => write!(f, "{}", self.code),
    }
  }
}

impl DataError {
  /// 创建验证失败错误（400）
  pub fn bad_request(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::BAD_REQUEST),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建资源未找到错误（404）
  pub fn not_found(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::NOT_FOUND),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建前置条件未满足错误（参数合法但系统状态不就绪，如必勾项未完成 / 已归档资源不可实例化）。
  /// 映射 gRPC/Connect `FailedPrecondition`；与 [`Self::bad_request`]（参数本身非法）语义区分。
  pub fn failed_precondition(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::FAILED_PRECONDITION),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建资源冲突错误（409）
  pub fn conflicted(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::CONFLICT),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建未认证错误（401）
  pub fn unauthorized(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::UNAUTHORIZED),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建权限拒绝错误（403）
  pub fn forbidden(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::PERMISSION_DENIED),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建服务器内部错误（500）
  pub fn server_error(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::INTERNAL_ERROR),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建功能未实现错误（501 / Connect `Unimplemented`）。与 503 区分：
  /// 重试不会成功，调用方不应重试。
  pub fn not_implemented(msg: impl Into<Cow<'static, str>>) -> Self {
    Self {
      code: Cow::Borrowed(codes::NOT_IMPLEMENTED),
      message: Some(msg.into()),
      request_id: None,
      details: None,
      source: None,
    }
  }

  /// 创建业务错误（自定义错误码）
  pub fn biz_error(
    code: impl Into<Cow<'static, str>>,
    msg: impl Into<Cow<'static, str>>,
    details: Option<serde_json::Value>,
  ) -> Self {
    Self {
      code: code.into(),
      message: Some(msg.into()),
      request_id: None,
      details: details.map(Box::new),
      source: None,
    }
  }

  /// 创建内部错误（带源错误）
  pub fn internal(
    code: impl Into<Cow<'static, str>>,
    msg: impl Into<Cow<'static, str>>,
    source: Option<Box<dyn core::error::Error + Send + Sync>>,
  ) -> Self {
    Self { code: code.into(), message: Some(msg.into()), request_id: None, details: None, source }
  }

  /// 创建重试超限错误
  pub fn retry_limit(msg: impl Into<Cow<'static, str>>, retry_limit: u32) -> Self {
    let details = serde_json::json!({ "retry_limit": retry_limit });
    Self {
      code: Cow::Borrowed(codes::RETRY_LIMIT),
      message: Some(msg.into()),
      request_id: None,
      details: Some(Box::new(details)),
      source: None,
    }
  }

  /// 设置请求 ID
  pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
    self.request_id = Some(request_id.into());
    self
  }

  /// 设置错误详情
  pub fn with_details(mut self, details: serde_json::Value) -> Self {
    self.details = Some(Box::new(details));
    self
  }

  /// 挂接源错误，保留错误链（供 `From` 实现链式补 `source`）。
  pub fn with_source(mut self, source: impl core::error::Error + Send + Sync + 'static) -> Self {
    self.source = Some(Box::new(source));
    self
  }
}

// ==========================================
// fusion-common
// ==========================================

impl From<CommonError> for DataError {
  fn from(value: CommonError) -> Self {
    DataError::server_error(value.to_string()).with_source(value)
  }
}

impl From<CtxError> for DataError {
  fn from(value: CtxError) -> Self {
    match value {
      CtxError::Unauthorized(msg) => DataError::unauthorized(msg),
      CtxError::InvalidPayload => DataError::bad_request("Invalid ctx payload"),
    }
  }
}

// ==========================================
// std / serde_json / chrono
// ==========================================

impl From<std::time::SystemTimeError> for DataError {
  fn from(value: std::time::SystemTimeError) -> Self {
    Self::internal(codes::INTERNAL_ERROR, "SystemTimeError", Some(Box::new(value)))
  }
}

impl From<std::io::Error> for DataError {
  fn from(value: std::io::Error) -> Self {
    let error_msg = value.to_string();
    DataError::internal(codes::IO_ERROR, format!("IO error: {}", error_msg), Some(Box::new(value)))
  }
}

impl From<serde_json::Error> for DataError {
  fn from(value: serde_json::Error) -> Self {
    DataError::internal(codes::INTERNAL_ERROR, "JSON error", Some(Box::new(value)))
  }
}

impl From<std::net::AddrParseError> for DataError {
  fn from(value: std::net::AddrParseError) -> Self {
    DataError::server_error(format!("Addr parse error: {}", value)).with_source(value)
  }
}

impl From<chrono::ParseError> for DataError {
  fn from(value: chrono::ParseError) -> Self {
    DataError::bad_request(format!("Parse date error: {}", value)).with_source(value)
  }
}

impl From<uuid::Error> for DataError {
  fn from(value: uuid::Error) -> Self {
    let msg = value.to_string();
    DataError::internal(codes::INTERNAL_ERROR, msg, Some(Box::new(value)))
  }
}

// ==========================================
// tokio
// ==========================================

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for DataError
where
  T: Send + Sync + 'static,
{
  fn from(e: tokio::sync::mpsc::error::SendError<T>) -> Self {
    let compatible_error: Box<dyn std::error::Error + Send + Sync + 'static> = Box::new(e);
    DataError::internal(codes::CHANNEL_ERROR, "channel send error", Some(compatible_error))
  }
}

impl From<tokio::sync::oneshot::error::RecvError> for DataError {
  fn from(e: tokio::sync::oneshot::error::RecvError) -> Self {
    let compatible_error: Box<dyn std::error::Error + Send + Sync + 'static> = Box::new(e);
    DataError::internal(codes::CHANNEL_ERROR, "channel recv error", Some(compatible_error))
  }
}

impl From<tokio::task::JoinError> for DataError {
  fn from(value: tokio::task::JoinError) -> Self {
    let compatible_error: Box<dyn std::error::Error + Send + Sync + 'static> = Box::new(value);
    DataError::internal(codes::INTERNAL_ERROR, "Join tokio task error", Some(compatible_error))
  }
}

// ==========================================
// mea
// ==========================================

impl<T> From<mea::mpsc::SendError<T>> for DataError
where
  T: Send + Sync + 'static,
{
  fn from(value: mea::mpsc::SendError<T>) -> Self {
    DataError::server_error(format!("Send to mea::mpsc error, {}", value)).with_source(value)
  }
}

// ==========================================
// config
// ==========================================

impl From<config::ConfigError> for DataError {
  fn from(value: config::ConfigError) -> Self {
    let msg = format!("Config load error: {}", value);
    DataError::internal(codes::CONFIG_ERROR, msg, Some(Box::new(value)))
  }
}

// ==========================================
// fusion-core
// ==========================================

impl From<fusion_core::component::ComponentError> for DataError {
  fn from(value: fusion_core::component::ComponentError) -> Self {
    DataError::internal(codes::INTERNAL_ERROR, value.to_string(), Some(Box::new(value)))
  }
}

impl From<fusion_core::configuration::ConfigureError> for DataError {
  fn from(value: fusion_core::configuration::ConfigureError) -> Self {
    DataError::server_error(value.to_string()).with_source(value)
  }
}

impl From<fusion_core::security::Error> for DataError {
  fn from(value: fusion_core::security::Error) -> Self {
    use fusion_core::security::Error as SecError;
    match value {
      SecError::TokenExpired => DataError::unauthorized("Token expired"),
      SecError::SignatureNotMatching => DataError::unauthorized("Signature not matching"),
      other => DataError::server_error(other.to_string()).with_source(other),
    }
  }
}

impl From<fusion_core::CoreError> for DataError {
  fn from(value: fusion_core::CoreError) -> Self {
    use fusion_core::CoreError;
    match value {
      CoreError::Component(e) => DataError::from(e),
      CoreError::Configure(e) => DataError::from(e),
      CoreError::Security(e) => DataError::from(e),
      CoreError::Io(e) => DataError::from(e),
      CoreError::TaskJoin(e) => DataError::from(e),
      CoreError::Tracing(msg) => DataError::server_error(msg),
      CoreError::Timer(msg) => DataError::server_error(msg),
      CoreError::Custom(msg) => DataError::server_error(msg),
    }
  }
}

// ==========================================
// fusion-rpc / connectrpc
// ==========================================

#[cfg(feature = "rpc")]
impl From<connectrpc::ConnectError> for DataError {
  fn from(value: connectrpc::ConnectError) -> Self {
    let msg = value.message.clone().unwrap_or_default();
    use connectrpc::ErrorCode;
    match value.code {
      ErrorCode::Canceled => DataError::internal(codes::RPC_ERROR, msg, None),
      ErrorCode::Unknown => DataError::internal(codes::RPC_ERROR, msg, None),
      ErrorCode::InvalidArgument => DataError::bad_request(msg),
      ErrorCode::DeadlineExceeded => DataError::internal(codes::SERVICE_UNAVAILABLE, msg, None),
      ErrorCode::NotFound => DataError::not_found(msg),
      ErrorCode::AlreadyExists => DataError::conflicted(msg),
      ErrorCode::PermissionDenied => DataError::forbidden(msg),
      ErrorCode::ResourceExhausted => DataError::internal(codes::RATE_LIMITED, msg, None),
      ErrorCode::FailedPrecondition => DataError::failed_precondition(msg),
      ErrorCode::Aborted => DataError::internal(codes::RPC_ERROR, msg, None),
      ErrorCode::OutOfRange => DataError::bad_request(msg),
      // Unimplemented 是永久性失败（重试无意义），不得映射为可重试的 503
      ErrorCode::Unimplemented => DataError::not_implemented(msg),
      ErrorCode::Internal => DataError::server_error(msg),
      ErrorCode::Unavailable => DataError::internal(codes::SERVICE_UNAVAILABLE, msg, None),
      ErrorCode::DataLoss => DataError::internal(codes::RPC_ERROR, msg, None),
      ErrorCode::Unauthenticated => DataError::unauthorized(msg),
      _ => DataError::internal(codes::RPC_ERROR, msg, None),
    }
  }
}

#[cfg(feature = "rpc")]
impl From<DataError> for connectrpc::ConnectError {
  fn from(value: DataError) -> Self {
    use connectrpc::ErrorCode;
    let code = match value.code.as_ref() {
      // validation 命名空间 -> InvalidArgument
      codes::BAD_REQUEST | codes::INVALID_ARGUMENT | codes::INVALID_PAYLOAD => ErrorCode::InvalidArgument,

      // validation.failed_precondition -> FailedPrecondition（参数合法但系统状态不就绪）
      codes::FAILED_PRECONDITION => ErrorCode::FailedPrecondition,

      // auth 命名空间 -> Unauthenticated / PermissionDenied
      codes::UNAUTHORIZED | codes::INVALID_TOKEN | codes::TOKEN_EXPIRED => ErrorCode::Unauthenticated,
      codes::PERMISSION_DENIED => ErrorCode::PermissionDenied,

      // resource 命名空间 -> NotFound / AlreadyExists
      codes::NOT_FOUND => ErrorCode::NotFound,
      codes::ALREADY_EXISTS | codes::CONFLICT => ErrorCode::AlreadyExists,

      // rate_limit 命名空间 -> ResourceExhausted
      codes::RATE_LIMITED | codes::RETRY_LIMIT => ErrorCode::ResourceExhausted,

      // system 命名空间 -> Internal / Unavailable / Unimplemented
      codes::INTERNAL_ERROR | codes::CHANNEL_ERROR | codes::RPC_ERROR => ErrorCode::Internal,
      codes::SERVICE_UNAVAILABLE | codes::CONFIG_ERROR | codes::IO_ERROR => ErrorCode::Unavailable,
      codes::NOT_IMPLEMENTED => ErrorCode::Unimplemented,

      // 默认
      _ => ErrorCode::Unknown,
    };
    let msg = value.message.unwrap_or_default();
    connectrpc::ConnectError::new(code, msg)
  }
}

// ==========================================
// fusion-db / fusion-sql / sqlx
// ==========================================

#[cfg(feature = "db")]
impl From<fusion_sql::SqlError> for DataError {
  fn from(value: fusion_sql::SqlError) -> Self {
    use fusion_sql::SqlError;
    match value {
      SqlError::Unauthorized(e) => DataError::unauthorized(e),
      SqlError::InvalidArgument { message } => DataError::bad_request(format!("InvalidArgument, {message}")),
      SqlError::EntityNotFound { schema, entity, id } => {
        DataError::not_found(format!("EntityNotFound, {}:{}:{}", schema.unwrap_or_default(), entity, id))
      }
      SqlError::NotFound { schema, table, sql } => {
        log::debug!("NotFound, schema: {}, table: {}, sql: {}", schema.unwrap_or_default(), table, sql);
        DataError::not_found(format!("NotFound, {}:{}", schema.unwrap_or_default(), table))
      }
      SqlError::ListLimitOverMax { max, actual } => {
        DataError::bad_request(format!("ListLimitOverMax, max: {max}, actual: {actual}"))
      }
      SqlError::ListLimitUnderMin { min, actual } => {
        DataError::bad_request(format!("ListLimitUnderMin, min: {min}, actual: {actual}"))
      }
      SqlError::ListPageUnderMin { min, actual } => {
        DataError::bad_request(format!("ListPageUnderMin, min: {min}, actual: {actual}"))
      }
      SqlError::UserAlreadyExists { key, value } => DataError::conflicted(format!("UserAlreadyExists, {key}:{value}")),
      SqlError::UniqueViolation { table, constraint } => {
        DataError::conflicted(format!("UniqueViolation, {table}:{constraint}"))
      }
      SqlError::ExecuteError { table, message } => {
        DataError::server_error(format!("ExecuteError, {}:{}", table, message))
      }
      SqlError::ExecuteFail { schema, table } => {
        DataError::server_error(format!("ExecuteFail, {:?}:{}", schema, table))
      }
      SqlError::CountFail { schema, table } => DataError::server_error(format!("CountFail, {:?}:{}", schema, table)),
      e @ SqlError::InvalidDatabase(_) => DataError::server_error(e.to_string()),
      e @ SqlError::CantCreateModelManagerProvider(_) => DataError::server_error(e.to_string()),
      // 调用侧装配缺陷（未 with_ctx 就做上下文相关操作），不是认证失败 → 500 而非 401
      e @ SqlError::CtxMissing => DataError::server_error(e.to_string()),
      e @ SqlError::JsonError(_) => DataError::server_error(e.to_string()),
      SqlError::Custom(msg) => DataError::server_error(msg),
      SqlError::DbxError(e) => DataError::internal(codes::IO_ERROR, e.to_string(), Some(Box::new(e))),
      SqlError::Sqlx(e) => DataError::internal(codes::IO_ERROR, e.to_string(), Some(Box::new(e))),
    }
  }
}

#[cfg(feature = "db")]
impl From<fusion_sql::store::DbxError> for DataError {
  fn from(value: fusion_sql::store::DbxError) -> Self {
    // 优先用 SQLSTATE 精确匹配（sqlx::Error::Database 的 Display 不输出 SQLSTATE，
    // 字符串 contains("23505") 永远 miss → 旧版本会把 UNIQUE 冲突误归为 server_error）
    if let fusion_sql::store::DbxError::Sqlx(sqlx_err) = &value
      && let Some(db_err) = sqlx_err.as_database_error()
      && let Some(code) = db_err.code()
    {
      let msg = value.to_string();
      return match code.as_ref() {
        "23505" => DataError::conflicted(msg),  // unique_violation
        "23503" => DataError::bad_request(msg), // foreign_key_violation
        _ => DataError::server_error(msg),
      };
    }

    // Fallback：非 sqlx 来源的 DbxError 走原字符串匹配（保留向后兼容）
    let msg = value.to_string();
    if msg.contains("23505") {
      DataError::conflicted(msg)
    } else if msg.contains("23503") {
      DataError::bad_request(msg)
    } else {
      DataError::server_error(msg)
    }
  }
}

#[cfg(feature = "db")]
impl From<sqlx::Error> for DataError {
  fn from(value: sqlx::Error) -> Self {
    DataError::internal(codes::IO_ERROR, format!("Sqlx error: {}", value), Some(Box::new(value)))
  }
}

// ==========================================
// fusion-web
// ==========================================

#[cfg(feature = "web")]
impl From<fusion_web::WebError> for DataError {
  fn from(value: fusion_web::WebError) -> Self {
    let code = if value.code.is_empty() { Cow::Borrowed(codes::INTERNAL_ERROR) } else { value.code };
    Self { code, message: value.message, request_id: value.request_id, details: value.details, source: None }
  }
}

#[cfg(feature = "web")]
impl From<DataError> for fusion_web::WebError {
  fn from(err: DataError) -> Self {
    if let Some(source) = err.source.as_ref() {
      log::error!("DataError with code {:?}, msg {:?} has source: {:?}", err.code, err.message, source);
    }
    fusion_web::WebError { code: err.code, message: err.message, request_id: err.request_id, details: err.details }
  }
}

// ==========================================
// fusion-security
// ==========================================

#[cfg(feature = "security")]
impl From<fusion_security::SecurityError> for DataError {
  fn from(value: fusion_security::SecurityError) -> Self {
    use fusion_security::SecurityError;
    match value {
      SecurityError::TokenGeneration => DataError::unauthorized("Failed to generate token"),
      SecurityError::TokenVerification(msg) => DataError::unauthorized(format!("Token verification failed: {msg}")),
      SecurityError::TokenExpired => DataError::unauthorized("Token expired"),
      SecurityError::InvalidToken => DataError::unauthorized("Invalid token format"),
      SecurityError::OAuth(msg) => DataError::unauthorized(format!("OAuth error: {msg}")),
      SecurityError::InvalidPassword => DataError::unauthorized("Invalid password"),
      SecurityError::FailedToVerifyPassword => DataError::unauthorized("Failed to verify password"),
      SecurityError::Core(e) => DataError::from(e),
      SecurityError::Custom(msg) => DataError::server_error(msg),
      // FailedToHashPassword / InvalidHashFormat / PasswordWorkerJoinFailed：
      // 服务端基础设施级错误，统一 server_error + source 保留错误链。
      other => DataError::server_error(other.to_string()).with_source(other),
    }
  }
}

// ==========================================
// fusion-weixin
// ==========================================

#[cfg(feature = "weixin")]
impl From<fusion_weixin::WeixinError> for DataError {
  fn from(value: fusion_weixin::WeixinError) -> Self {
    use fusion_weixin::WeixinError;
    match value {
      // 换码被拒（code 无效 / 凭据无效等请求侧错误）→ 401（消费方统一
      // 「第三方凭证无效或已过期」族文案）。
      WeixinError::Invalid { .. } => DataError::unauthorized(value.to_string()),
      // 出站依赖不可用（通道未配置 / 网络不可达 / 系统忙 / 分钟配额）→ 503（瞬态）。
      WeixinError::Unavailable { .. } => {
        DataError::internal(codes::SERVICE_UNAVAILABLE, value.to_string(), Some(Box::new(value)))
      }
      // unionid 强锚失败（配置缺陷）→ 服务端错误（非用户侧可修复）。
      WeixinError::MissingUnionid => DataError::server_error(value.to_string()).with_source(value),
    }
  }
}

// ==========================================
// fusion-ai
// ==========================================

#[cfg(feature = "ai")]
impl From<fusion_ai::AiError> for DataError {
  fn from(value: fusion_ai::AiError) -> Self {
    let msg = value.to_string();
    // 上游连接失败 / provider 5xx/429 → 503（瞬态，可重试）；
    // 请求构造 / 响应解析 / 本地缺陷 → 500（重试无意义）。
    // 分级判据的唯一真相源 = `AiError::is_upstream_transient`（fusion-ai-de-rig.md §4.3）。
    if value.is_upstream_transient() {
      DataError::internal(codes::SERVICE_UNAVAILABLE, msg, Some(Box::new(value)))
    } else {
      DataError::server_error(msg).with_source(value)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn not_implemented_code_survives_connect_round_trip() {
    #[cfg(feature = "rpc")]
    {
      use connectrpc::{ConnectError, ErrorCode};

      // DataError → ConnectError：501 语义映射 Unimplemented，而非 Unknown
      let data_err = DataError::not_implemented("rpc not implemented");
      let connect_err: ConnectError = data_err.into();
      assert_eq!(connect_err.code, ErrorCode::Unimplemented);

      // ConnectError → DataError：Unimplemented 不得映射为可重试的 503
      let back: DataError = ConnectError::new(ErrorCode::Unimplemented, "nope").into();
      assert_eq!(back.code.as_ref(), codes::NOT_IMPLEMENTED);
    }
  }

  #[cfg(feature = "ai")]
  #[test]
  fn ai_error_maps_transient_upstream_to_service_unavailable() {
    use fusion_ai::AiError;
    use fusion_ai::providers::openai_compatible::errors::OpenAiCompatError;

    // provider 5xx / 429 / 连接层错误 → 503（可重试），并保留错误链
    let transient_cases = [
      OpenAiCompatError::Http { status: 429, message: "rate limited".into() },
      OpenAiCompatError::Http { status: 503, message: "upstream down".into() },
      OpenAiCompatError::Transport("connect refused".into()),
    ];
    for case in transient_cases {
      let err: DataError = AiError::OpenAiCompat(case).into();
      assert_eq!(err.code.as_ref(), codes::SERVICE_UNAVAILABLE, "transient must map to 503");
      assert!(core::error::Error::source(&err).is_some(), "source chain must be preserved");
    }

    // 本地缺陷（响应解析 / 请求构造 / 4xx）→ 500
    let local_cases = [
      OpenAiCompatError::ResponseParse("bad json".into()),
      OpenAiCompatError::RequestBuild("missing field".into()),
      OpenAiCompatError::Http { status: 400, message: "bad request".into() },
      OpenAiCompatError::Stream("mid-stream".into()),
    ];
    for case in local_cases {
      let err: DataError = AiError::OpenAiCompat(case).into();
      assert_eq!(err.code.as_ref(), codes::INTERNAL_ERROR, "local defect must map to 500");
      assert!(core::error::Error::source(&err).is_some(), "source chain must be preserved");
    }

    // AiError::Custom → 500
    let err: DataError = AiError::Custom("plain failure".into()).into();
    assert_eq!(err.code.as_ref(), codes::INTERNAL_ERROR);
  }

  #[cfg(feature = "db")]
  #[test]
  fn sql_ctx_missing_maps_to_internal_not_unauthorized() {
    // 未 with_ctx 是装配缺陷 → 500；不得伪装成 401 误导排障方向
    let err: DataError = fusion_sql::SqlError::CtxMissing.into();
    assert_eq!(err.code.as_ref(), codes::INTERNAL_ERROR);
  }
}
