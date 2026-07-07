use connectrpc::{ConnectError, ErrorCode, RequestContext};
use http::HeaderMap;

use fusion_common::ctx::CtxPayload;
use fusion_core::{configuration::SecuritySetting, security::SecurityUtils};

/// 从 ConnectRPC RequestContext 中提取认证载荷
#[allow(clippy::result_large_err)]
pub fn extract_payload_from_context(sc: &SecuritySetting, ctx: &RequestContext) -> Result<CtxPayload, ConnectError> {
  let token = extract_token_from_headers(ctx.headers())?;
  let (payload, _) = SecurityUtils::decrypt_jwt(sc.pwd(), token)
    .map_err(|e| ConnectError::new(ErrorCode::Unauthenticated, e.to_string()))?;
  Ok(payload)
}

/// 从 HTTP headers 中提取 Bearer token
#[allow(clippy::result_large_err)]
pub fn extract_token_from_headers(headers: &HeaderMap) -> Result<&str, ConnectError> {
  let auth_header = headers
    .get("authorization")
    .ok_or_else(|| ConnectError::new(ErrorCode::Unauthenticated, "Missing authorization header"))?;
  let auth_str = auth_header
    .to_str()
    .map_err(|_| ConnectError::new(ErrorCode::Unauthenticated, "Invalid authorization header"))?;
  auth_str
    .strip_prefix("Bearer ")
    .ok_or_else(|| ConnectError::new(ErrorCode::Unauthenticated, "Expected Bearer authorization scheme"))
}

/// 解析 UUID 字符串，失败返回 ConnectError
#[allow(clippy::result_large_err)]
pub fn parse_uuid(s: &str) -> Result<uuid::Uuid, ConnectError> {
  uuid::Uuid::parse_str(s).map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid uuid: {}", e)))
}

/// Parse ConnectRPC path into (service, method) parts.
/// e.g., "/hylx.auth.v1.AuthService/Login" → ("hylx.auth.v1.AuthService", "Login")
///
/// 返回 borrow 而非 owned String：所有 caller（auth_middleware /
/// context_validation_middleware / hylx-core middleware）拿到后立即比较 +
/// 立即丢弃，借用就够，无需为 hot path 每请求分配 2 个 String。
pub fn parse_rpc_path(path: &str) -> Option<(&str, &str)> {
  let path = path.trim_start_matches('/');
  let slash_pos = path.rfind('/')?;
  let service = &path[..slash_pos];
  let method = &path[slash_pos + 1..];
  if service.contains('.') && !method.is_empty() { Some((service, method)) } else { None }
}

/// Extension trait for ergonomic conversion of common error types into
/// [`ConnectError`].
///
/// 业务 handler 反复写 `.map_err(|e| ConnectError::new(ErrorCode::Internal, e.to_string()))`
/// 的样板。本 trait 让 caller 改写 `.map_err(IntoConnectError::into_internal)` 即可。
///
/// 边界收窄（防错误码丢失）：
/// - blanket impl 仅覆盖实现了 [`std::error::Error`] 的类型 —— 真正的错误类型，
///   而非任意 `Display`（`Display` 太宽，会把 `String` / 普通枚举也卷进来）。
/// - [`ConnectError`] 自身**也**实现 `std::error::Error`，故同样被 blanket 覆盖；
///   blanket 的 `into_internal` 对一个已带精确错误码（`NotFound` /
///   `InvalidArgument` …）的 `ConnectError` 会无脑降级成 `Internal`。要保留原
///   错误码的调用方应改用 [`ConnectErrorPassthrough::into_connect_error`]
///   （对 `ConnectError` 原样返回，不降级）。
///
/// 后续如有更多错误码（`into_not_found` / `into_invalid_argument`）按需扩展。
pub trait IntoConnectError {
  /// Map this error into [`ConnectError`] with [`ErrorCode::Internal`].
  fn into_internal(self) -> ConnectError;
}

impl<E: std::error::Error> IntoConnectError for E {
  fn into_internal(self) -> ConnectError {
    ConnectError::new(ErrorCode::Internal, self.to_string())
  }
}

/// Passthrough conversion for values that are already a [`ConnectError`].
///
/// Unlike [`IntoConnectError::into_internal`], this preserves the original
/// [`ErrorCode`] — use it when an error may already be a `ConnectError`
/// carrying a precise code that must not be flattened to `Internal`.
///
/// Implemented as a *separate* trait (not a specialised
/// `impl IntoConnectError for ConnectError`) because `ConnectError` itself
/// implements [`std::error::Error`] and is therefore already covered by the
/// `IntoConnectError` blanket — a second impl would be a coherence conflict
/// (E0119).
pub trait ConnectErrorPassthrough {
  /// Return the value as a [`ConnectError`] without altering its error code.
  fn into_connect_error(self) -> ConnectError;
}

impl ConnectErrorPassthrough for ConnectError {
  fn into_connect_error(self) -> ConnectError {
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn into_internal_maps_std_error_to_internal_code() {
    let io_err = std::io::Error::other("disk gone");
    let ce = io_err.into_internal();
    assert_eq!(ce.code, ErrorCode::Internal);
  }

  #[test]
  fn passthrough_preserves_precise_error_code() {
    let original = ConnectError::new(ErrorCode::NotFound, "resident missing");
    let code = original.code;
    let passed = original.into_connect_error();
    // The precise NotFound code survives — not flattened to Internal.
    assert_eq!(passed.code, code);
    assert_eq!(passed.code, ErrorCode::NotFound);
  }

  #[test]
  fn parse_rpc_path_splits_service_and_method() {
    assert_eq!(parse_rpc_path("/hylx.auth.v1.AuthService/Login"), Some(("hylx.auth.v1.AuthService", "Login")));
    assert_eq!(parse_rpc_path("/no-dot/Method"), None);
    assert_eq!(parse_rpc_path("/incomplete"), None);
  }
}
