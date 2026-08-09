//! AuthLayer middleware — decrypts JWE token and injects trusted identity headers.
//!
//! A framework-level layer that handles JWT decryption and header injection.
//! All business-specific values (exempt paths, claim-to-header mappings, error messages)
//! are provided via `AuthConfig`.

use axum::body::Body;
use fusion_core::configuration::SecuritySetting;
use fusion_core::security::SecurityUtils;
use http::{Request, Response, StatusCode};
use std::future::Future;
use std::pin::Pin;
use tower_http::auth::{AsyncAuthorizeRequest, AsyncRequireAuthorizationLayer};

use crate::utils::parse_rpc_path;

/// Describes how to extract a JWT claim and inject it as a request header.
#[derive(Clone, Copy, Debug)]
pub struct ClaimMapping {
  /// The HTTP header name to inject (e.g., "x-tenant-id").
  pub header: &'static str,
  /// How to read the claim from the JWT payload.
  pub source: ClaimSource,
}

/// How to extract a value from the JWT payload.
#[derive(Clone, Copy, Debug)]
pub enum ClaimSource {
  /// JWT standard "sub" claim — uses `get_subject()`.
  Subject,
  /// Custom string claim — uses `get_str(claim_name)`.
  String(&'static str),
  /// Custom integer claim — uses `get_i64(claim_name)`.
  I64(&'static str),
  /// Try string first, fallback to integer.
  StringOrI64(&'static str),
}

/// A non-user principal that an application-owned OUTER layer has already verified — e.g. a
/// signed cross-service scope from a sibling bin whose background job holds no user token.
///
/// Carried as a request **extension**, never a header. An HTTP client can set any header, but it
/// cannot set a request extension: extensions are in-process typed slots. That matters because
/// `AuthLayer` is the outermost layer, so if this arrived as a header it would have no way to tell
/// "injected by my own inner layer" from "sent by the caller" — the extension removes that
/// undecidable case by construction.
///
/// `AuthLayer` still strips every `claim_mappings` header first (a forged `x-tenant-id` dies
/// there); the identity headers below are what gets re-injected.
#[derive(Clone, Debug)]
pub struct TrustedSubject {
  /// Who vouched for this principal (goes into logs / audit), e.g. `"consumer-bin:system"`.
  pub principal: String,
  /// Identity headers to inject downstream, e.g. `[("x-tenant-id", "3")]`. The verifying layer
  /// derives these from whatever it verified — `AuthLayer` never invents them.
  pub identity_headers: Vec<(&'static str, String)>,
}

/// Configuration for the auth middleware.
///
/// All fields use `&'static str` since values are known at compile time and
/// configured once at application startup.
#[derive(Clone, Copy, Debug)]
pub struct AuthConfig {
  /// Path prefixes that skip authentication (e.g., `["/health", "/config"]`).
  pub exclude_paths: &'static [&'static str],
  /// Exempt path prefixes that intentionally consume configured identity
  /// headers themselves. For these paths AuthLayer skips both auth and the
  /// usual pre-auth header stripping.
  pub preserve_identity_headers_for_paths: &'static [&'static str],
  /// (service, method) ConnectRPC pairs that skip authentication.
  pub exclude_rpcs: &'static [(&'static str, &'static str)],
  /// (service, method) pairs a verified [`TrustedSubject`] may reach.
  ///
  /// Fail-closed and deliberately separate from [`Self::exclude_rpcs`]: an entry here does NOT
  /// make the RPC anonymous — a caller without either a bearer token or a trusted subject is
  /// still rejected, and a trusted subject calling anything outside this list is rejected too.
  pub trusted_subject_rpcs: &'static [(&'static str, &'static str)],
  /// JWT claim → HTTP header mappings.
  pub claim_mappings: &'static [ClaimMapping],
  /// Name of the cookie carrying the JWT, used as a fallback when no
  /// `Authorization: Bearer` header is present. Framework default is the
  /// business-agnostic `access_token`; applications override it with their
  /// own cookie name (see [`AuthConfig::DEFAULT`]).
  pub cookie_token_name: &'static str,
  /// Error code in the JSON response body for auth failures.
  pub error_code: &'static str,
  /// Fallback error message in the JSON response body.
  pub error_message: &'static str,
}

impl AuthConfig {
  /// A baseline config with framework-default values. Applications spread it
  /// to inherit defaults (notably `cookie_token_name = "access_token"`) while
  /// overriding the business-specific fields:
  ///
  /// ```ignore
  /// let cfg = AuthConfig { claim_mappings: MY_MAPPINGS, ..AuthConfig::DEFAULT };
  /// ```
  pub const DEFAULT: AuthConfig = AuthConfig {
    exclude_paths: &[],
    preserve_identity_headers_for_paths: &[],
    exclude_rpcs: &[],
    trusted_subject_rpcs: &[],
    claim_mappings: &[],
    cookie_token_name: "access_token",
    error_code: "unauthenticated",
    error_message: "Invalid or expired token",
  };
}

impl Default for AuthConfig {
  fn default() -> Self {
    Self::DEFAULT
  }
}

/// AuthLayer extracts JWT claims and injects trusted identity headers.
///
/// Business-agnostic — all specifics come from `AuthConfig`.
#[derive(Clone)]
pub struct AuthLayer {
  security: SecuritySetting,
  config: AuthConfig,
}

impl AuthLayer {
  pub fn new(security: SecuritySetting, config: AuthConfig) -> Self {
    Self { security, config }
  }

  pub fn into_middleware(self) -> AsyncRequireAuthorizationLayer<AuthAuthorizer> {
    AsyncRequireAuthorizationLayer::new(AuthAuthorizer { security: self.security, config: self.config })
  }
}

/// The actual authorizer that processes each request.
#[derive(Clone)]
pub struct AuthAuthorizer {
  security: SecuritySetting,
  config: AuthConfig,
}

impl AsyncAuthorizeRequest<Body> for AuthAuthorizer {
  type RequestBody = Body;
  type ResponseBody = Body;
  type Future = Pin<Box<dyn Future<Output = Result<Request<Body>, Response<Self::ResponseBody>>> + Send>>;

  fn authorize(&mut self, mut request: Request<Body>) -> Self::Future {
    let security = self.security.clone();
    let config = self.config;
    Box::pin(async move {
      let path = request.uri().path().to_string();
      let preserve_identity_headers =
        config.preserve_identity_headers_for_paths.iter().any(|prefix| path.starts_with(prefix));
      if !preserve_identity_headers {
        strip_configured_identity_headers(&mut request, &config);
      }

      // Check path-based exemptions
      if config.exclude_paths.iter().any(|prefix| path.starts_with(prefix)) {
        return Ok(request);
      }

      // Check ConnectRPC service/method exemptions
      if let Some((service, method)) = parse_rpc_path(&path)
        && config.exclude_rpcs.iter().any(|(s, m)| *s == service && *m == method)
      {
        return Ok(request);
      }

      // A non-user principal an inner-trusted layer already verified. Admitted only for the
      // explicitly whitelisted RPCs; anything else falls through to the bearer requirement below,
      // so the default answer for a trusted subject is the same 401 an anonymous caller gets.
      if let Some(subject) = request.extensions().get::<TrustedSubject>().cloned() {
        let allowed = parse_rpc_path(&path)
          .map(|(service, method)| config.trusted_subject_rpcs.iter().any(|(s, m)| *s == service && *m == method))
          .unwrap_or(false);
        if allowed {
          let headers = request.headers_mut();
          for (name, value) in &subject.identity_headers {
            match value.parse() {
              Ok(hv) => {
                headers.insert(*name, hv);
              }
              Err(_) => {
                // Same fail-closed rule as the claim path: a header the downstream cannot read
                // would let it fall through to its "no tenant" branch.
                log::warn!(
                  target: "fusion_rpc::auth",
                  "auth: rejecting trusted subject '{}' — identity header '{name}' is not valid ASCII",
                  subject.principal
                );
                return Err(unauthorized_response(&config, config.error_message));
              }
            }
          }
          log::debug!(
            target: "fusion_rpc::auth",
            "auth: admitted trusted subject '{}' for {path} (metric=fusion_rpc.auth.trusted_subject_admitted)",
            subject.principal
          );
          return Ok(request);
        }
        log::warn!(
          target: "fusion_rpc::auth",
          "auth: trusted subject '{}' is not whitelisted for {path} (metric=fusion_rpc.auth.trusted_subject_refused)",
          subject.principal
        );
      }

      // Extract and decrypt JWT
      let token = extract_bearer_token(request.headers(), config.cookie_token_name)
        .map_err(|msg| unauthorized_response(&config, &msg))?;

      let (payload, _) = SecurityUtils::decrypt_jwt(security.pwd(), &token)
        .map_err(|_| unauthorized_response(&config, config.error_message))?;

      // Inject trusted identity headers from JWT claims
      let headers = request.headers_mut();
      for mapping in config.claim_mappings {
        let value: Option<String> = match mapping.source {
          ClaimSource::Subject => payload.get_subject().map(|s| s.to_string()),
          ClaimSource::String(claim) => payload.get_str(claim).map(|s| s.to_string()),
          ClaimSource::I64(claim) => payload.get_i64(claim).map(|v| v.to_string()),
          ClaimSource::StringOrI64(claim) => {
            if let Some(s) = payload.get_str(claim) {
              Some(s.to_string())
            } else {
              payload.get_i64(claim).map(|v| v.to_string())
            }
          }
        };
        if let Some(v) = value {
          match v.parse() {
            Ok(hv) => {
              headers.insert(mapping.header, hv);
            }
            Err(_) => {
              // Fail closed: invalid claim → reject the request rather than
              // silently dropping the header. Without this, an attacker
              // crafting a non-ASCII tenant_id claim could let the downstream
              // service run on its fallback (missing x-tenant-id often falls
              // through to "no RLS" / "system" path) and cross-tenant read.
              log::warn!(
                target: "fusion_rpc::auth",
                "auth: rejecting request — claim for '{}' contains invalid ASCII / unparseable value",
                mapping.header
              );
              return Err(unauthorized_response(&config, config.error_message));
            }
          }
        }
      }

      Ok(request)
    })
  }
}

fn strip_configured_identity_headers(request: &mut Request<Body>, config: &AuthConfig) {
  let headers = request.headers_mut();
  for mapping in config.claim_mappings {
    headers.remove(mapping.header);
  }
}

// Anti-forgery note: AuthLayer strips & re-injects the headers in
// `claim_mappings`, but **does not** strip caller-supplied `Cookie` entries.
// Rationale: cookie auth fallback path lives in `extract_bearer_token` which
// only reads the cookie named by `AuthConfig::cookie_token_name`; the consumer's
// app-side ctx layer downstream reads identity exclusively from the trusted `x-*-id` headers
// re-injected by this layer, never from cookies. That said, callers running
// mixed auth paths should ensure their downstream services do not consume
// identity claims from `Cookie`. See `extract_cookie_value` below for the
// single-cookie semantics (split by ';', take first matching name=value pair,
// no de-dup of multiple same-named cookies — Cookie spec leaves precedence
// undefined; matching first name occurrence is the conservative read).

/// Extract Bearer token from Authorization header, falling back to the
/// configured cookie (`cookie_token_name`) when no header is present.
fn extract_bearer_token(headers: &http::HeaderMap, cookie_token_name: &str) -> Result<String, String> {
  if let Some(auth_header) = headers.get("authorization") {
    let auth_str = auth_header.to_str().map_err(|_| "Invalid authorization header".to_string())?;
    let token = auth_str
      .strip_prefix("Bearer ")
      .ok_or_else(|| "Invalid authorization scheme, expected Bearer".to_string())?;
    return Ok(token.to_string());
  }

  extract_cookie_value(headers, cookie_token_name).ok_or_else(|| "Missing authorization header".to_string())
}

fn extract_cookie_value(headers: &http::HeaderMap, name: &str) -> Option<String> {
  let cookie = headers.get("cookie")?.to_str().ok()?;
  cookie.split(';').find_map(|part| {
    let (key, value) = part.trim().split_once('=')?;
    if key == name && !value.is_empty() { Some(value.to_string()) } else { None }
  })
}

/// Build an unauthorized response from config.
fn unauthorized_response(config: &AuthConfig, msg: &str) -> Response<Body> {
  let body = serde_json::json!({"code": config.error_code, "message": msg}).to_string();
  Response::builder()
    .status(StatusCode::UNAUTHORIZED)
    .header(http::header::CONTENT_TYPE, "application/json")
    .body(Body::from(body))
    .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use fusion_common::ctx::CtxPayload;
  use fusion_core::configuration::SecuritySetting;
  use std::time::{Duration, SystemTime};

  fn test_config() -> AuthConfig {
    AuthConfig {
      exclude_paths: &["/health", "/config", "/version"],
      preserve_identity_headers_for_paths: &[],
      exclude_rpcs: &[("myapp.auth.v1.AuthService", "Login"), ("myapp.auth.v1.AuthService", "RefreshToken")],
      trusted_subject_rpcs: &[("myapp.permission.v1.PermissionService", "ListUsersByPermission")],
      claim_mappings: &[
        ClaimMapping { header: "x-tenant-id", source: ClaimSource::String("tenant_id") },
        ClaimMapping { header: "x-user-id", source: ClaimSource::Subject },
        ClaimMapping { header: "x-facility-id", source: ClaimSource::StringOrI64("facility_id") },
        ClaimMapping { header: "x-context-type", source: ClaimSource::String("context_type") },
      ],
      cookie_token_name: "app_access_token",
      error_code: "unauthenticated",
      error_message: "Invalid or expired token",
    }
  }

  fn test_security() -> SecuritySetting {
    serde_json::from_str(
      r#"{"pwd":{"secret_key":"0123456789ABCDEF0123456789ABCDEF","expires_in":7200,"default_pwd":"test"},"token":{"secret_key":"0123456789ABCDEF0123456789ABCDEF","expires_in":7200,"public_key":"","private_key":""}}"#,
    ).expect("SecuritySetting deserialization should not fail")
  }

  fn make_test_token(security: &SecuritySetting, payload: CtxPayload) -> String {
    SecurityUtils::encrypt_jwt(security.pwd(), payload).expect("encrypt_jwt failed")
  }

  fn make_request(path: &str, headers: Vec<(&str, &str)>) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(path);
    for (key, value) in headers {
      builder = builder.header(key, value);
    }
    builder.body(Body::empty()).unwrap()
  }

  fn make_payload() -> CtxPayload {
    let mut payload = CtxPayload::default();
    payload.set_subject("user-42");
    payload.set_string("tenant_id", "100");
    payload.set_string("facility_id", "fac-1");
    payload.set_string("context_type", "facility");
    payload.set_exp(
      (SystemTime::now() + Duration::from_secs(600))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64,
    );
    payload
  }

  #[tokio::test]
  async fn test_valid_token_injects_headers() {
    let security = test_security();
    let token = make_test_token(&security, make_payload());
    let mut authorizer = AuthAuthorizer { security: security.clone(), config: test_config() };
    let req = make_request(
      "/myapp.resident.v1.ResidentService/ListResidents",
      vec![("authorization", format!("Bearer {}", token).as_str())],
    );
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
    let req = result.unwrap();
    let headers = req.headers();
    assert_eq!(headers.get("x-tenant-id").unwrap(), "100");
    assert_eq!(headers.get("x-user-id").unwrap(), "user-42");
    assert_eq!(headers.get("x-facility-id").unwrap(), "fac-1");
    assert_eq!(headers.get("x-context-type").unwrap(), "facility");
  }

  #[tokio::test]
  async fn test_exempt_rpc_strips_spoofed_identity_headers() {
    let security = test_security();
    let mut authorizer = AuthAuthorizer { security: security.clone(), config: test_config() };
    let req = make_request(
      "/myapp.auth.v1.AuthService/Login",
      vec![
        ("x-tenant-id", "forged-tenant"),
        ("x-user-id", "forged-user"),
        ("x-facility-id", "forged-facility"),
        ("x-context-type", "facility"),
      ],
    );
    let result = authorizer.authorize(req).await.unwrap();
    let headers = result.headers();
    assert!(headers.get("x-tenant-id").is_none());
    assert!(headers.get("x-user-id").is_none());
    assert!(headers.get("x-facility-id").is_none());
    assert!(headers.get("x-context-type").is_none());
  }

  #[tokio::test]
  async fn test_preserved_exempt_path_keeps_identity_headers() {
    let security = test_security();
    let config = AuthConfig {
      exclude_paths: &["/health", "/config", "/version", "/agent-api/"],
      preserve_identity_headers_for_paths: &["/agent-api/"],
      ..test_config()
    };
    let mut authorizer = AuthAuthorizer { security: security.clone(), config };
    let req = make_request(
      "/agent-api/myapp.provider_credential.v1.ProviderCredentialService/ListProviders",
      vec![("x-tenant-id", "tenant-1"), ("x-user-id", "user-1")],
    );
    let result = authorizer.authorize(req).await.unwrap();
    let headers = result.headers();
    assert_eq!(headers.get("x-tenant-id").unwrap(), "tenant-1");
    assert_eq!(headers.get("x-user-id").unwrap(), "user-1");
  }

  #[tokio::test]
  async fn test_string_or_i64_fallback_to_i64() {
    let security = test_security();
    let mut payload = CtxPayload::default();
    payload.set_i64("facility_id", 999);
    payload.set_exp(
      (SystemTime::now() + Duration::from_secs(600))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64,
    );
    let token = make_test_token(&security, payload);
    let mut authorizer = AuthAuthorizer { security: security.clone(), config: test_config() };
    let req = make_request(
      "/myapp.resident.v1.ResidentService/ListResidents",
      vec![("authorization", format!("Bearer {}", token).as_str())],
    );
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().headers().get("x-facility-id").unwrap(), "999");
  }

  #[tokio::test]
  async fn test_exempt_path_passes() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req = make_request("/health", vec![]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_exempt_rpc_passes() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req = make_request("/myapp.auth.v1.AuthService/Login", vec![]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_missing_authorization_returns_401() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req = make_request("/myapp.resident.v1.ResidentService/ListResidents", vec![]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
  }

  #[tokio::test]
  async fn test_invalid_bearer_scheme_returns_401() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req =
      make_request("/myapp.resident.v1.ResidentService/ListResidents", vec![("authorization", "Basic dXNlcjpwYXNz")]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
  }

  #[tokio::test]
  async fn test_cookie_token_uses_configured_name() {
    let security = test_security();
    let token = make_test_token(&security, make_payload());
    let mut authorizer = AuthAuthorizer { security: security.clone(), config: test_config() };
    let req = make_request(
      "/myapp.resident.v1.ResidentService/ListResidents",
      vec![("cookie", format!("app_access_token={}", token).as_str())],
    );
    let result = authorizer.authorize(req).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().headers().get("x-tenant-id").unwrap(), "100");
  }

  #[tokio::test]
  async fn test_cookie_token_wrong_name_returns_401() {
    let security = test_security();
    let token = make_test_token(&security, make_payload());
    let mut authorizer = AuthAuthorizer { security: security.clone(), config: test_config() };
    // Cookie present but under the framework-default name, while config
    // expects `app_access_token` — must be treated as missing auth.
    let req = make_request(
      "/myapp.resident.v1.ResidentService/ListResidents",
      vec![("cookie", format!("access_token={}", token).as_str())],
    );
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().status(), StatusCode::UNAUTHORIZED);
  }

  #[test]
  fn test_default_cookie_token_name_is_business_agnostic() {
    assert_eq!(AuthConfig::DEFAULT.cookie_token_name, "access_token");
    assert_eq!(AuthConfig::default().cookie_token_name, "access_token");
  }

  // ---- TrustedSubject (ADR-0003 in the consuming application) ----

  fn trusted_subject_request(path: &str) -> Request<Body> {
    let mut req = make_request(path, vec![("x-tenant-id", "forged-tenant")]);
    req.extensions_mut().insert(TrustedSubject {
      principal: "sibling-bin:system".to_string(),
      identity_headers: vec![("x-tenant-id", "3".to_string()), ("x-context-type", "tenant".to_string())],
    });
    req
  }

  #[tokio::test]
  async fn test_trusted_subject_admitted_only_for_whitelisted_rpc() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req = trusted_subject_request("/myapp.permission.v1.PermissionService/ListUsersByPermission");
    let result = authorizer.authorize(req).await.expect("whitelisted trusted subject is admitted");
    // The forged inbound header was stripped and replaced by the subject's own value.
    assert_eq!(result.headers().get("x-tenant-id").unwrap(), "3");
    assert_eq!(result.headers().get("x-context-type").unwrap(), "tenant");
    // A trusted subject carries no user identity.
    assert!(result.headers().get("x-user-id").is_none());
  }

  #[tokio::test]
  async fn test_trusted_subject_outside_whitelist_still_401() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req = trusted_subject_request("/myapp.resident.v1.ResidentService/ListResidents");
    let result = authorizer.authorize(req).await;
    assert!(result.is_err(), "an off-whitelist RPC MUST NOT be reachable by a trusted subject");
    assert_eq!(result.unwrap_err().status(), StatusCode::UNAUTHORIZED);
  }

  #[tokio::test]
  async fn test_whitelisted_rpc_without_trusted_subject_still_401() {
    // The whitelist does NOT make the RPC anonymous.
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req = make_request("/myapp.permission.v1.PermissionService/ListUsersByPermission", vec![]);
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().status(), StatusCode::UNAUTHORIZED);
  }

  #[tokio::test]
  async fn test_trusted_subject_with_unrepresentable_header_is_refused() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let mut req = make_request("/myapp.permission.v1.PermissionService/ListUsersByPermission", vec![]);
    req.extensions_mut().insert(TrustedSubject {
      principal: "sibling-bin:system".to_string(),
      // A control character can never be a header value (http rejects it) — the point is that an
      // unrepresentable value fails closed instead of being silently dropped, which would let the
      // downstream fall through to its "no tenant" branch.
      identity_headers: vec![("x-tenant-id", "3\n4".to_string())],
    });
    let result = authorizer.authorize(req).await;
    assert!(result.is_err(), "an unparseable identity header MUST fail closed, not be dropped");
  }

  #[tokio::test]
  async fn test_invalid_token_returns_401() {
    let mut authorizer = AuthAuthorizer { security: test_security(), config: test_config() };
    let req = make_request(
      "/myapp.resident.v1.ResidentService/ListResidents",
      vec![("authorization", "Bearer not-a-valid-token")],
    );
    let result = authorizer.authorize(req).await;
    assert!(result.is_err());
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
  }
}
