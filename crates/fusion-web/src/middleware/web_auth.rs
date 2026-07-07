use std::sync::Arc;

use axum::body::Body;
use axum::extract::Query;
use fusion_common::ctx::Ctx;
use fusion_common::time::now_offset;
use fusion_core::application::Application;
use fusion_core::security::AccessToken;
use fusion_core::utils::get_trace_id;
use headers::{Cookie, HeaderMapExt};
use http::request::Parts;
use http::{Request, StatusCode, header::AUTHORIZATION, header::CONTENT_TYPE};
use tower_http::auth::{AsyncAuthorizeRequest, AsyncRequireAuthorizationLayer};

use crate::extract_ctx;
use crate::{WebError, middleware::web_error_to_response};

#[derive(Clone)]
struct ExternalUrl {
  url: Arc<String>,
  client: reqwest::Client,
}

/// WebAuth is a middleware that checks if the request is authorized by calling remote API.
///
/// 路径过滤求值顺序（见 [`AsyncAuthorizeRequest::authorize`] 实现）：
/// 1. 先判 `includes`：**空列表 = 不限制**（所有路径都视为需鉴权）；
///    非空时，仅前缀匹配 `includes` 中任一项的路径需鉴权，其余路径直接 401。
/// 2. 再判 `excludes`：**空列表 = 不排除**；非空时，前缀匹配 `excludes`
///    中任一项的路径直接 401（excludes 在 includes 之后求值，即使路径
///    命中 includes，只要也命中 excludes 同样会被拒）。
#[derive(Clone, Default)]
pub struct WebAuth {
  /// A list of url paths that must be accessed with authentication.
  /// 空列表表示不限制（所有路径都需鉴权）。
  includes: Arc<Vec<String>>,
  /// A list of url paths that must not be accessed with authentication.
  /// 空列表表示不排除任何路径。
  excludes: Arc<Vec<String>>,
  external: Option<ExternalUrl>,
}

impl WebAuth {
  /// Set the API base URL.
  pub fn with_api_base_url(mut self, api_base_url: &str) -> Self {
    self.external = Some(ExternalUrl {
      url: Arc::new(format!("{}/api/auth/extract_token", api_base_url.trim_end_matches('/'))),
      client: reqwest::Client::new(),
    });
    self
  }

  /// Add includes paths.
  pub fn with_includes(mut self, includes: Vec<String>) -> Self {
    self.includes = Arc::new(includes);
    self
  }

  /// Add excludes paths.
  pub fn with_excludes(mut self, excludes: Vec<String>) -> Self {
    self.excludes = Arc::new(excludes);
    self
  }

  pub fn into_layer(self) -> AsyncRequireAuthorizationLayer<Self> {
    AsyncRequireAuthorizationLayer::new(self)
  }
}

impl AsyncAuthorizeRequest<Body> for WebAuth {
  type RequestBody = Body;
  type ResponseBody = Body;
  type Future = futures::future::BoxFuture<'static, Result<http::Request<Body>, http::Response<Self::ResponseBody>>>;

  fn authorize(&mut self, request: http::Request<Self::RequestBody>) -> Self::Future {
    let path = request.uri().path().to_string();
    // Check if path is in includes (if includes is not empty)
    if !self.includes.is_empty() && !self.includes.iter().any(|include| path.starts_with(include)) {
      return Box::pin(async move {
        Err(web_error_to_response(
          WebError::unauthorized(format!("Url path `{}` is not in includes", path)),
          StatusCode::UNAUTHORIZED,
        ))
      });
    }

    // Check if path is in excludes (if excludes is not empty)
    if !self.excludes.is_empty() && self.excludes.iter().any(|exclude| path.starts_with(exclude)) {
      return Box::pin(async move {
        Err(web_error_to_response(
          WebError::unauthorized(format!("Url path `{}` is in excludes", path)),
          StatusCode::UNAUTHORIZED,
        ))
      });
    }

    match self.external.as_ref() {
      Some(external) => {
        let url = external.url.clone();
        let client = external.client.clone();
        Box::pin(async move {
          let (mut parts, body) = request.into_parts();
          let ctx = validate_token_remote(&url, &client, &parts)
            .await
            .map_err(|e| web_error_to_response(e, StatusCode::UNAUTHORIZED))?;
          parts.extensions.insert(ctx);
          Ok(Request::from_parts(parts, body))
        })
      }
      None => Box::pin(async {
        let (mut parts, body) = request.into_parts();
        // TODO: 考虑在更早的时候获取 &SecuritySetting
        let ctx = extract_ctx(&parts, Application::global().fusion_setting().security())
          .map_err(|e| web_error_to_response(e, StatusCode::UNAUTHORIZED))?;
        parts.extensions.insert(ctx);
        Ok(Request::from_parts(parts, body))
      }),
    }
  }
}

/// Call remote API to validate token and extract context
async fn validate_token_remote(url: &str, client: &reqwest::Client, parts: &Parts) -> Result<Ctx, WebError> {
  let mut req_builder = client.post(url).header(CONTENT_TYPE, "application/json");

  if let Some(authorization_value) = parts.headers.get(AUTHORIZATION) {
    req_builder = req_builder.header(AUTHORIZATION, authorization_value);
  } else if let Some(cookie) = parts.headers.typed_get::<Cookie>()
    && let Some(token) = cookie.get("access_token")
  {
    req_builder = req_builder.bearer_auth(token);
  } else if let Ok(ac) = Query::<AccessToken>::try_from_uri(&parts.uri) {
    // 受限场景兜底：query string `access_token`。token 会进 access log /
    // Referer / 浏览器历史，仅用于无法走 header/cookie 的 WebSocket / 下载直链。
    req_builder = req_builder.bearer_auth(ac.access_token.as_str());
  } else {
    return Err(WebError::unauthorized("Authentication failed, missing token"));
  }

  let response = req_builder
    .send()
    .await
    .map_err(|e| WebError::unauthorized(format!("Failed to call auth API: {}", e)))?;

  if response.status().is_success() {
    let response_data: serde_json::Value = response
      .json()
      .await
      .map_err(|e| WebError::unauthorized(format!("Failed to parse auth response: {}", e)))?;

    // Convert response data to CtxPayload
    let response_map = response_data
      .as_object()
      .ok_or_else(|| WebError::unauthorized("Invalid token response format"))?
      .clone();

    let ctx_payload = fusion_common::ctx::CtxPayload::from(response_map);

    // Create Ctx from payload
    let ctx = Ctx::try_new(ctx_payload, Some(now_offset()), get_trace_id())
      .map_err(|e| WebError::unauthorized(format!("Failed to create context: {}", e)))?;

    Ok(ctx)
  } else {
    let status = response.status();
    let error_msg = format!("Token validation failed: {}", status);
    Err(WebError::unauthorized(error_msg))
  }
}
