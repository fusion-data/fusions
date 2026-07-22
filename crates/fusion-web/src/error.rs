use std::borrow::Cow;

use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use fusion_core::configuration::ConfigureError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type WebResult<T> = core::result::Result<Json<T>, WebError>;

/// Web 错误响应体
///
/// 遵循 SPECIFICATION.md 错误响应体规范：
/// - `code`: 字符串类型，格式 `namespace.error_name`（snake_case）
/// - `message`: 可选，面向调试但不得包含敏感明文
/// - `request_id`: 可选，请求追踪 ID
/// - `details`: 可选，附加错误详情
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
  feature = "with-openapi",
  derive(utoipa::ToSchema, utoipa::ToResponse),
  response(description = "A default error response for most API errors.")
)]
pub struct WebError {
  /// 错误码，格式：`namespace.error_name`（snake_case）
  pub code: Cow<'static, str>,

  /// 可选，错误消息
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<Cow<'static, str>>,

  /// 可选，请求追踪 ID
  #[serde(skip_serializing_if = "Option::is_none")]
  pub request_id: Option<String>,

  /// 可选，附加错误详情（Box 包装减小结构体大小）
  #[serde(skip_serializing_if = "Option::is_none")]
  pub details: Option<Box<Value>>,
}

impl WebError {
  /// 创建新的错误
  pub fn new(code: impl Into<Cow<'static, str>>, msg: impl Into<Cow<'static, str>>) -> Self {
    Self { code: code.into(), message: Some(msg.into()), request_id: None, details: None }
  }

  /// 创建服务器错误（默认 500）
  pub fn server_error(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::INTERNAL_ERROR, msg)
  }

  /// 设置请求 ID
  pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
    self.request_id = Some(request_id.into());
    self
  }

  /// 设置错误详情。
  ///
  /// 与 [`Self::with_request_id`] 语义对齐：caller 传什么就存什么，不做"贴心"
  /// 空值清理（之前会把 `Value::Null` 静默丢成 `None`，与 `with_request_id("")`
  /// 不丢的行为不对称，调试时 caller 设进去找不回来反而更困惑）。如需清空请
  /// caller 自行判断后不调本方法。
  #[must_use]
  pub fn with_details(mut self, details: Value) -> Self {
    self.details = Some(Box::new(details));
    self
  }

  // ==========================================
  // 4xx Client Errors
  // ==========================================

  /// 400 - 请求格式错误
  pub fn bad_request(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::BAD_REQUEST, msg)
  }

  /// 401 - 未认证
  pub fn unauthorized(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::UNAUTHORIZED, msg)
  }

  /// 403 - 权限拒绝
  pub fn forbidden(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::PERMISSION_DENIED, msg)
  }

  /// 404 - 资源未找到
  pub fn not_found(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::NOT_FOUND, msg)
  }

  /// 409 - 资源冲突
  pub fn conflict(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::CONFLICT, msg)
  }

  /// 422 - 无法处理的实体
  pub fn unprocessable_entity(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new("validation.unprocessable_entity", msg)
  }

  /// 429 - 请求过多
  pub fn too_many_requests(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::RATE_LIMITED, msg)
  }

  // ==========================================
  // 5xx Server Errors
  // ==========================================

  /// 501 - 未实现
  pub fn not_implemented(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::NOT_IMPLEMENTED, msg)
  }

  /// 502 - 网关错误
  pub fn bad_gateway(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new("system.bad_gateway", msg)
  }

  /// 503 - 服务不可用
  pub fn service_unavailable(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new(fusion_common::codes::SERVICE_UNAVAILABLE, msg)
  }

  /// 504 - 网关超时
  pub fn gateway_timeout(msg: impl Into<Cow<'static, str>>) -> Self {
    Self::new("system.gateway_timeout", msg)
  }
}

impl IntoResponse for WebError {
  fn into_response(self) -> axum::response::Response {
    use fusion_common::codes::*;

    // 根据错误码映射 HTTP 状态码
    let status = match self.code.as_ref() {
      // validation 命名空间 -> 400
      BAD_REQUEST | INVALID_ARGUMENT | INVALID_PAYLOAD => StatusCode::BAD_REQUEST,

      // auth 命名空间
      UNAUTHORIZED | INVALID_TOKEN | TOKEN_EXPIRED => StatusCode::UNAUTHORIZED,
      PERMISSION_DENIED => StatusCode::FORBIDDEN,

      // resource 命名空间
      NOT_FOUND => StatusCode::NOT_FOUND,
      ALREADY_EXISTS | CONFLICT => StatusCode::CONFLICT,

      // rate_limit 命名空间 -> 429
      RATE_LIMITED | RETRY_LIMIT => StatusCode::TOO_MANY_REQUESTS,

      // system 命名空间 -> 5xx
      INTERNAL_ERROR => StatusCode::INTERNAL_SERVER_ERROR,
      SERVICE_UNAVAILABLE | CONFIG_ERROR | IO_ERROR => StatusCode::SERVICE_UNAVAILABLE,
      // not_implemented / bad_gateway / gateway_timeout 之前落入 _ → 500，与
      // builder 函数的 doc/name（501/502/504）不一致 → caller 看 500 误以为是
      // 自己服务挂了，实际是"该 RPC 未实现 / 上游网关问题"。明确映射。
      NOT_IMPLEMENTED => StatusCode::NOT_IMPLEMENTED,
      "system.bad_gateway" => StatusCode::BAD_GATEWAY,
      "system.gateway_timeout" => StatusCode::GATEWAY_TIMEOUT,

      // 默认
      _ => StatusCode::INTERNAL_SERVER_ERROR,
    };

    let mut res = axum::Json(self).into_response();
    *res.status_mut() = status;
    res
  }
}

// `From<XError> for WebError` 不向客户端泄露 inner error 的 Display：
// std::io::Error / ConfigureError / hyper::Error 等 Display 可能含路径、用户名、
// 内部地址等敏感信息，response body 的 message 字段会回到前端。改成中性
// "internal error" 让客户端只看到错误码，详细堆栈走 `tracing::error!` 落到
// 服务侧日志。SPECIFICATION.md 明确 `message` 字段不得包含敏感明文。
impl From<std::io::Error> for WebError {
  fn from(value: std::io::Error) -> Self {
    tracing::error!(error = %value, "WebError::from(io::Error)");
    WebError::server_error("internal io error")
  }
}

impl From<ConfigureError> for WebError {
  fn from(value: ConfigureError) -> Self {
    tracing::error!(error = %value, "WebError::from(ConfigureError)");
    WebError::server_error("internal config error")
  }
}

impl From<hyper::Error> for WebError {
  fn from(value: hyper::Error) -> Self {
    tracing::error!(error = %value, "WebError::from(hyper::Error)");
    WebError::server_error("internal transport error")
  }
}

impl From<serde_json::Error> for WebError {
  fn from(value: serde_json::Error) -> Self {
    tracing::error!(error = %value, "WebError::from(serde_json::Error)");
    WebError::server_error("internal serialization error")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::http::StatusCode;
  use axum::response::IntoResponse;

  #[test]
  fn test_http_error_functions() {
    // Test 4xx client errors
    let error = WebError::bad_request("Invalid request");
    assert_eq!(error.code.as_ref(), fusion_common::codes::BAD_REQUEST);
    assert_eq!(error.message.as_deref(), Some("Invalid request"));

    let error = WebError::unauthorized("Unauthorized access");
    assert_eq!(error.code.as_ref(), fusion_common::codes::UNAUTHORIZED);
    assert_eq!(error.message.as_deref(), Some("Unauthorized access"));

    let error = WebError::forbidden("Access forbidden");
    assert_eq!(error.code.as_ref(), fusion_common::codes::PERMISSION_DENIED);
    assert_eq!(error.message.as_deref(), Some("Access forbidden"));

    let error = WebError::not_found("Resource not found");
    assert_eq!(error.code.as_ref(), fusion_common::codes::NOT_FOUND);
    assert_eq!(error.message.as_deref(), Some("Resource not found"));

    let error = WebError::conflict("Resource conflict");
    assert_eq!(error.code.as_ref(), fusion_common::codes::CONFLICT);
    assert_eq!(error.message.as_deref(), Some("Resource conflict"));

    let error = WebError::unprocessable_entity("Unprocessable entity");
    assert_eq!(error.code.as_ref(), "validation.unprocessable_entity");
    assert_eq!(error.message.as_deref(), Some("Unprocessable entity"));

    let error = WebError::too_many_requests("Rate limit exceeded");
    assert_eq!(error.code.as_ref(), fusion_common::codes::RATE_LIMITED);
    assert_eq!(error.message.as_deref(), Some("Rate limit exceeded"));

    // Test 5xx server errors
    let error = WebError::server_error("Internal server error");
    assert_eq!(error.code.as_ref(), fusion_common::codes::INTERNAL_ERROR);
    assert_eq!(error.message.as_deref(), Some("Internal server error"));

    let error = WebError::not_implemented("Feature not implemented");
    assert_eq!(error.code.as_ref(), "system.not_implemented");
    assert_eq!(error.message.as_deref(), Some("Feature not implemented"));

    let error = WebError::bad_gateway("Bad gateway");
    assert_eq!(error.code.as_ref(), "system.bad_gateway");
    assert_eq!(error.message.as_deref(), Some("Bad gateway"));

    let error = WebError::service_unavailable("Service unavailable");
    assert_eq!(error.code.as_ref(), fusion_common::codes::SERVICE_UNAVAILABLE);
    assert_eq!(error.message.as_deref(), Some("Service unavailable"));

    let error = WebError::gateway_timeout("Gateway timeout");
    assert_eq!(error.code.as_ref(), "system.gateway_timeout");
    assert_eq!(error.message.as_deref(), Some("Gateway timeout"));
  }

  #[test]
  fn test_into_response() {
    let error = WebError::not_found("Resource not found");
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let error = WebError::server_error("Server error");
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let error = WebError::bad_request("Invalid input");
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let error = WebError::unauthorized("Unauthorized");
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let error = WebError::forbidden("Forbidden");
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let error = WebError::conflict("Conflict");
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let error = WebError::too_many_requests("Too many requests");
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
  }
}
