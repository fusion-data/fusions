use std::borrow::Cow;

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::http::request::Parts;
use headers::authorization::Bearer;
use headers::{Authorization, Cookie, HeaderMapExt};
use serde::de::DeserializeOwned;
#[cfg(feature = "with-ulid")]
use ulid::Ulid;

use fusion_common::ctx::Ctx;
use fusion_common::model::IdI64Result;
use fusion_common::time::now_offset;
use fusion_core::configuration::SecuritySetting;
use fusion_core::security::{AccessToken, SecurityUtils};
use fusion_core::utils::get_trace_id;

use crate::WebResult;
use crate::error::WebError;

/// ok_json! 宏：支持无参数（返回 Ok(Json(()))）或一个参数（返回 Ok(Json(v))）
#[macro_export]
macro_rules! ok_json {
  () => {
    Ok(axum::Json(().into()))
  };
  ($v:expr) => {
    Ok(axum::Json($v))
  };
}

#[inline]
pub fn ok_id(id: i64) -> WebResult<IdI64Result> {
  Ok(IdI64Result::new(id).into())
}

#[cfg(feature = "with-ulid")]
#[inline]
pub fn ok_ulid(id: Ulid) -> WebResult<fusion_common::model::IdUlidResult> {
  Ok(fusion_common::model::IdUlidResult::new(id).into())
}

#[inline]
pub fn ok_uuid(id: uuid::Uuid) -> WebResult<fusion_common::model::IdUuidResult> {
  Ok(fusion_common::model::IdUuidResult::new(id).into())
}

pub fn unauthorized_app_error(msg: impl Into<String>) -> (StatusCode, Json<WebError>) {
  (StatusCode::UNAUTHORIZED, Json(WebError::unauthorized(Cow::Owned(msg.into()))))
}

/// 按优先级从请求中提取 access token：
/// 1. `Authorization: Bearer <token>` header（首选）；
/// 2. `access_token` cookie；
/// 3. URL query string `?access_token=<token>`。
///
/// **安全警告**：query string 中的 token 会进入服务端 access log、被带入
/// `Referer` header、并留在浏览器历史 / 代理缓存中，存在泄露风险。该路径仅用于
/// 无法走 header / cookie 的受限场景（如 WebSocket 升级请求、`<a download>`
/// 直链下载）。常规请求务必优先走 header 或 cookie。
pub fn extract_token(parts: &Parts) -> Result<String, WebError> {
  if let Some(Authorization(bearer)) = parts.headers.typed_get::<Authorization<Bearer>>() {
    Ok(bearer.token().to_string())
  } else if let Some(cookie) = parts.headers.typed_get::<Cookie>()
    && let Some(value) = cookie.get("access_token")
  {
    Ok(value.to_string())
  } else if let Ok(at) = Query::<AccessToken>::try_from_uri(&parts.uri) {
    // 受限场景兜底（WebSocket / 下载直链）；见函数级安全警告。
    Ok(at.0.access_token)
  } else {
    Err(WebError::unauthorized("Missing token"))
  }
}

/// 从 Http Request Authorization Header 或 access_token query 中获取 [Ctx]
pub fn extract_ctx(parts: &Parts, sc: &SecuritySetting) -> Result<Ctx, WebError> {
  let req_time = now_offset();

  let token = extract_token(parts)?;
  let (payload, _) =
    SecurityUtils::decrypt_jwt(sc.pwd(), &token).map_err(|_e| WebError::unauthorized("Failed decode jwt"))?;

  let ctx = Ctx::try_new(payload, Some(req_time), get_trace_id()).map_err(|e| WebError::unauthorized(e.to_string()))?;
  Ok(ctx)
}

pub fn extensions_2_ctx(parts: &Parts) -> Result<&Ctx, WebError> {
  let ctx = parts
    .extensions
    .get()
    .ok_or_else(|| WebError::unauthorized("The current login session does not exist, No Ctx found."))?;
  Ok(ctx)
}

pub fn opt_to_web_result<T>(opt: Option<T>) -> WebResult<T>
where
  T: DeserializeOwned,
{
  if let Some(v) = opt { Ok(Json(v)) } else { Err(WebError::not_found("Not found.")) }
}
