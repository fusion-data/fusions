use axum::{
  Form,
  body::Body,
  extract::{
    FromRequest,
    rejection::{FormRejection, JsonRejection},
  },
  http::{Request, header},
};
use headers::ContentType;
use mime::Mime;
use serde::de::DeserializeOwned;

use crate::WebError;

/// 二选一请求体 extractor —— 按 `Content-Type` 解析为 JSON 或 form。
///
/// 契约：
/// - `application/json` → 走 [`axum::Json`] 反序列化为 `T`。
/// - `application/x-www-form-urlencoded` → 走 [`axum::Form`] 反序列化为 `T`。
/// - 缺少 `Content-Type` header → 返回 `400 Bad Request`。
/// - `Content-Type` 非 ASCII / 无法解析为 MIME → 返回 `400 Bad Request`。
/// - `Content-Type` 是上述两者之外的值 → 返回 `400 Bad Request`。
///
/// 所有 rejection 都映射为 [`WebError::bad_request`]。
pub struct JsonOrForm<T>(pub T);

impl<S, T> FromRequest<S> for JsonOrForm<T>
where
  S: Send + Sync,
  T: DeserializeOwned,
{
  type Rejection = WebError;

  async fn from_request(req: Request<Body>, state: &S) -> Result<Self, Self::Rejection> {
    let header_value =
      req.headers().get(header::CONTENT_TYPE).ok_or(WebError::bad_request("'Content-Type' not found"))?;

    let content_type: ContentType = header_value
      .to_str()
      .map_err(|ex| WebError::bad_request(ex.to_string()))?
      .parse()
      .map_err(|_ex| WebError::bad_request("'Content-Type' invalid"))?;

    let m: Mime = content_type.into();

    let res = if mime::APPLICATION_JSON == m {
      let axum::Json(res) = axum::Json::<T>::from_request(req, state)
        .await
        .map_err(|ex: JsonRejection| WebError::bad_request(ex.body_text()))?;
      res
    } else if mime::APPLICATION_WWW_FORM_URLENCODED == m {
      let Form(res) = Form::<T>::from_request(req, state)
        .await
        .map_err(|ex: FormRejection| WebError::bad_request(ex.body_text()))?;
      res
    } else {
      return Err(WebError::bad_request(format!(
        "Unsupported Content-Type for JsonOrForm extractor: {m} (expected application/json or application/x-www-form-urlencoded)"
      )));
    };
    Ok(JsonOrForm(res))
  }
}
