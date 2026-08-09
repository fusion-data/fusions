//! ContextValidationLayer middleware — generic conditional header validation.
//!
//! A framework-level layer that enforces: when a context header has a specific value,
//! a required header must be present and non-empty. All business-specific values
//! (header names, exempt paths, error messages) are provided via `ContextValidationConfig`.

use axum::body::Body;
use http::{Request, Response, StatusCode};
use std::future::Future;
use std::pin::Pin;
use tower_http::auth::{AsyncAuthorizeRequest, AsyncRequireAuthorizationLayer};

use crate::utils::parse_rpc_path;

/// Configuration for context-based header validation.
///
/// All fields use `&'static str` since values are known at compile time and
/// configured once at application startup.
#[derive(Clone)]
pub struct ContextValidationConfig {
  /// Header name to read the context type from (e.g., "x-context-type").
  pub context_header: &'static str,
  /// When `context_header` has this value, the required header must be present (e.g., "facility").
  pub trigger_value: &'static str,
  /// Header that must be present and non-empty when triggered (e.g., "x-facility-id").
  pub require_header: &'static str,
  /// Path prefixes that skip validation (e.g., `["/health", "/config", "/version"]`).
  pub exclude_paths: &'static [&'static str],
  /// (service, method) ConnectRPC pairs that skip validation.
  pub exclude_rpcs: &'static [(&'static str, &'static str)],
  /// HTTP status code for rejection (e.g., 403).
  pub reject_status: u16,
  /// Error code in the JSON response body.
  pub error_code: &'static str,
  /// Error message in the JSON response body.
  pub error_message: &'static str,
}

/// Generic middleware layer for conditional header validation.
///
/// Validates that when a context header matches a trigger value, a required header
/// is present and non-empty. Business-agnostic — all specifics come from `ContextValidationConfig`.
#[derive(Clone)]
pub struct ContextValidationLayer {
  config: ContextValidationConfig,
}

impl ContextValidationLayer {
  pub fn new(config: ContextValidationConfig) -> Self {
    Self { config }
  }

  pub fn into_middleware(self) -> AsyncRequireAuthorizationLayer<ContextValidationAuthorizer> {
    AsyncRequireAuthorizationLayer::new(ContextValidationAuthorizer { config: self.config })
  }
}

/// The actual authorizer that validates context headers.
#[derive(Clone)]
pub struct ContextValidationAuthorizer {
  config: ContextValidationConfig,
}

impl AsyncAuthorizeRequest<Body> for ContextValidationAuthorizer {
  type RequestBody = Body;
  type ResponseBody = Body;
  type Future = Pin<Box<dyn Future<Output = Result<Request<Body>, Response<Self::ResponseBody>>> + Send>>;

  fn authorize(&mut self, request: Request<Body>) -> Self::Future {
    let config = self.config.clone();
    Box::pin(async move {
      let path = request.uri().path().to_string();

      // Skip exempt paths
      if config.exclude_paths.iter().any(|prefix| path.starts_with(prefix)) {
        return Ok(request);
      }

      // Skip exempt RPCs
      if let Some((service, method)) = parse_rpc_path(&path)
        && config.exclude_rpcs.iter().any(|(s, m)| *s == service && *m == method)
      {
        return Ok(request);
      }

      let headers = request.headers();

      // Read context header
      let context_type = headers.get(config.context_header).and_then(|v| v.to_str().ok()).unwrap_or("");

      // Only validate when context matches trigger value
      if context_type == config.trigger_value {
        let required_value = headers.get(config.require_header).and_then(|v| v.to_str().ok()).unwrap_or("");

        if required_value.is_empty() {
          return Err(reject_response(&config));
        }
      }

      // Non-trigger context or missing context header → pass through
      Ok(request)
    })
  }
}

/// Build a rejection response from config.
fn reject_response(config: &ContextValidationConfig) -> Response<Body> {
  let body = serde_json::json!({
    "code": config.error_code,
    "message": config.error_message
  })
  .to_string();
  Response::builder()
    .status(StatusCode::from_u16(config.reject_status).unwrap_or(StatusCode::FORBIDDEN))
    .header(http::header::CONTENT_TYPE, "application/json")
    .body(Body::from(body))
    .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::Body;
  use http::{Request, StatusCode};

  fn test_config() -> ContextValidationConfig {
    ContextValidationConfig {
      context_header: "x-context-type",
      trigger_value: "facility",
      require_header: "x-facility-id",
      exclude_paths: &["/health", "/config", "/version"],
      exclude_rpcs: &[("myapp.auth.v1.AuthService", "Login"), ("myapp.auth.v1.AuthService", "SetActiveContext")],
      reject_status: 403,
      error_code: "permission_denied",
      error_message: "facility context mismatch",
    }
  }

  fn make_request(path: &str, headers: Vec<(&str, &str)>) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(path);
    for (key, value) in headers {
      builder = builder.header(key, value);
    }
    builder.body(Body::empty()).unwrap()
  }

  #[tokio::test]
  async fn test_triggered_with_required_header_passes() {
    let config = test_config();
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request(
      "/myapp.resident.v1.ResidentService/ListResidents",
      vec![("x-context-type", "facility"), ("x-facility-id", "facility-123")],
    );
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_triggered_without_required_header_rejects() {
    let config = test_config();
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request("/myapp.resident.v1.ResidentService/ListResidents", vec![("x-context-type", "facility")]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
  }

  #[tokio::test]
  async fn test_non_trigger_context_passes() {
    let config = test_config();
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request("/myapp.resident.v1.ResidentService/ListResidents", vec![("x-context-type", "personal")]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_missing_context_header_passes() {
    let config = test_config();
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request("/myapp.resident.v1.ResidentService/ListResidents", vec![]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_exempt_path_passes() {
    let config = test_config();
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request("/health", vec![("x-context-type", "facility")]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_exempt_rpc_passes() {
    let config = test_config();
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request("/myapp.auth.v1.AuthService/SetActiveContext", vec![("x-context-type", "facility")]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_triggered_with_empty_required_header_rejects() {
    let config = test_config();
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request(
      "/myapp.resident.v1.ResidentService/ListResidents",
      vec![("x-context-type", "facility"), ("x-facility-id", "")],
    );
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
  }

  #[tokio::test]
  async fn test_custom_reject_status() {
    let mut config = test_config();
    config.reject_status = 401;
    config.error_code = "unauthorized";
    config.error_message = "missing tenant context";
    let mut authorizer = ContextValidationAuthorizer { config };
    let req = make_request("/myapp.resident.v1.ResidentService/ListResidents", vec![("x-context-type", "facility")]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
  }
}
