use axum::{body::Body, response::Response};
use http::{StatusCode, header::CONTENT_TYPE};

use crate::WebError;

/// Serialize a [`WebError`] to a JSON `Response<Body>` with the given HTTP status.
///
/// 之前函数硬编码 `StatusCode::UNAUTHORIZED` 但函数名暗示通用，让非 auth caller
/// 误把 ValidationError / InternalError 走此路径都翻成 401。改为接收 `status`
/// 让 caller 显式选错误码。
pub fn web_error_to_response(e: WebError, status: StatusCode) -> Response<Body> {
  let body = serde_json::to_vec(&e).unwrap_or_else(|_| b"{}".to_vec());
  Response::builder()
    .status(status)
    .header(CONTENT_TYPE, "application/json; charset=utf-8")
    .body(Body::from(body))
    .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Backward-compat wrapper: build an UNAUTHORIZED (401) JSON body.
/// New callers prefer [`web_error_to_response`] with explicit status.
#[deprecated(note = "Use web_error_to_response(e, StatusCode::UNAUTHORIZED) for explicit status")]
pub fn web_error_2_body(e: WebError) -> Response<Body> {
  web_error_to_response(e, StatusCode::UNAUTHORIZED)
}
