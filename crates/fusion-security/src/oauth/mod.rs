use oauth2::http::{Response, StatusCode};
use oauth2::{
  AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet, HttpRequest,
  HttpResponse, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl, basic::BasicClient,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// 进程级共享的 `reqwest::Client` —— 复用连接池，避免每次请求新建 client 丢弃池。
/// `Client` 内部是 `Arc`，clone 廉价。
fn shared_http_client() -> reqwest::Client {
  static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
  CLIENT.get_or_init(reqwest::Client::new).clone()
}

#[derive(Error, Debug)]
pub enum OAuthError {
  #[error("OAuth configuration error: {0}")]
  ConfigurationError(String),
  #[error("Token exchange error: {0}")]
  TokenExchangeError(String),
  #[error("User info error: {0}")]
  UserInfoError(String),
  #[error("HTTP request error: {0}")]
  RequestError(String),
  #[error("Unsupported OAuth provider: {0}")]
  UnsupportedProvider(String),
  /// The `state` returned on the OAuth callback did not match the value
  /// issued in `get_authorize_url` — a CSRF / authorization-code-injection
  /// signal. The exchange MUST be aborted.
  #[error("OAuth state mismatch — possible CSRF attack")]
  StateMismatch,
}

/// Constant-time byte-slice equality.
///
/// `exchange_code` compares the callback `state` against the issued one; a
/// naive `==` short-circuits on the first differing byte, leaking match-prefix
/// length via timing. This compares every byte regardless. Length mismatch is
/// itself a non-match — slices of differing length are never equal — and the
/// length check is not timing-sensitive (the issued `state` length is not a
/// secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
  if a.len() != b.len() {
    return false;
  }
  let mut diff: u8 = 0;
  for (x, y) in a.iter().zip(b.iter()) {
    diff |= x ^ y;
  }
  diff == 0
}

// 自定义 HTTP 客户端
async fn http_client(request: HttpRequest) -> Result<HttpResponse, OAuthError> {
  let client = shared_http_client();

  let method = match request.method().as_str() {
    "GET" => reqwest::Method::GET,
    "POST" => reqwest::Method::POST,
    "PUT" => reqwest::Method::PUT,
    "DELETE" => reqwest::Method::DELETE,
    _ => return Err(OAuthError::RequestError(format!("Unsupported method: {}", request.method()))),
  };

  let mut req_builder = client.request(method, request.uri().to_string());

  let mut has_user_agent = false;
  for (name, value) in request.headers() {
    if name.as_str().to_lowercase() == "user-agent" {
      has_user_agent = true;
    }
    let v = value.to_str().map_err(|e| OAuthError::RequestError(format!("header {name} not ASCII: {e}")))?;
    req_builder = req_builder.header(name.as_str(), v);
  }

  // Add User-Agent header if not present (GitHub requires this)
  if !has_user_agent {
    req_builder = req_builder.header("User-Agent", "fusion-security-oauth/0.1.0");
  }

  if !request.body().is_empty() {
    req_builder = req_builder.body(request.body().clone());
  }

  let response = req_builder.send().await.map_err(|e| OAuthError::RequestError(e.to_string()))?;

  let status = response.status().as_u16();

  let body = response.bytes().await.map_err(|e| OAuthError::RequestError(e.to_string()))?.to_vec();

  Response::builder()
    .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
    .body(body)
    .map_err(|e| OAuthError::RequestError(format!("build response failed: {e}")))
}

pub type OAuthResult<T> = Result<T, OAuthError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
  pub client_id: String,
  pub client_secret: String,
  pub redirect_url: String,
  pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
  pub id: String,
  pub username: String,
  pub name: Option<String>,
  pub email: Option<String>,
  pub avatar_url: Option<String>,
  pub provider: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthTokenResponse {
  pub access_token: String,
  pub token_type: String,
  pub expires_in: Option<u64>,
  pub refresh_token: Option<String>,
  pub scope: Option<String>,
}

impl std::fmt::Debug for OAuthTokenResponse {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // Mask token material; default derive would leak access_token /
    // refresh_token into `tracing::error!("{:?}", resp)` / panic backtrace.
    f.debug_struct("OAuthTokenResponse")
      .field("access_token", &"<REDACTED>")
      .field("token_type", &self.token_type)
      .field("expires_in", &self.expires_in)
      .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<REDACTED>"))
      .field("scope", &self.scope)
      .finish()
  }
}

/// Read current Unix epoch seconds.
///
/// `SystemTime::now().duration_since(UNIX_EPOCH)` 在系统时间被 NTP / VM resume
/// 回拨到 1970 之前会返 Err。此函数在该情况下返回 0 —— **过期判定会失真**
/// （`expires_at` 与 `is_expired_or_expiring_soon` 都基于此值，时钟早于
/// UNIX_EPOCH 时数学结果不可信）。仅记 `tracing::error!` 让运维能 alert
/// "clock skew"，不在此 panic / 中断流程。
fn now_unix_secs() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_else(|e| {
      tracing::error!(
        "oauth: system time before UNIX_EPOCH ({e}); falling back to 0 — token expiration math will be off"
      );
      Duration::ZERO
    })
    .as_secs()
}

impl OAuthTokenResponse {
  /// 获取令牌过期时间戳（秒）
  pub fn expires_at(&self) -> Option<u64> {
    self.expires_in.map(|duration| now_unix_secs() + duration)
  }

  /// 检查令牌是否已过期或即将过期（提前30秒）
  pub fn is_expired_or_expiring_soon(&self) -> bool {
    if let Some(expires_at) = self.expires_at() {
      let now = now_unix_secs();
      // 提前30秒判断为即将过期
      now + 30 >= expires_at
    } else {
      // 如果没有过期时间信息，假设不会过期
      false
    }
  }
}

/// 存储 OAuth 令牌的 trait
pub trait TokenStore: Send + Sync {
  /// 存储令牌
  fn store_token(&self, user_id: &str, token: OAuthTokenResponse) -> OAuthResult<()>;

  /// 获取令牌
  fn get_token(&self, user_id: &str) -> OAuthResult<Option<OAuthTokenResponse>>;

  /// 删除令牌
  fn remove_token(&self, user_id: &str) -> OAuthResult<()>;

  /// 获取所有用户的令牌
  fn list_tokens(&self) -> OAuthResult<Vec<(String, OAuthTokenResponse)>>;

  /// 获取为 Any 的引用，用于向下转型
  fn as_any(&self) -> &dyn std::any::Any;
}

/// 内存令牌存储实现
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
  tokens: Arc<RwLock<HashMap<String, (OAuthTokenResponse, Instant)>>>,
}

impl MemoryTokenStore {
  pub fn new() -> Self {
    Self::default()
  }

  /// 清理过期令牌
  pub fn cleanup_expired(&self) -> OAuthResult<usize> {
    // PoisonError 是逻辑 bug（其他线程持锁 panic）而非 ConfigurationError；
    // recover via into_inner 让进程继续工作而非把内部状态映射为配置错误。
    let mut tokens = self.tokens.write().unwrap_or_else(|p| p.into_inner());
    let initial_count = tokens.len();

    tokens.retain(|_, (token, _)| !token.is_expired_or_expiring_soon());

    Ok(initial_count - tokens.len())
  }
}

impl TokenStore for MemoryTokenStore {
  fn store_token(&self, user_id: &str, token: OAuthTokenResponse) -> OAuthResult<()> {
    // PoisonError 是逻辑 bug（其他线程持锁 panic）而非 ConfigurationError；
    // recover via into_inner 让进程继续工作而非把内部状态映射为配置错误。
    let mut tokens = self.tokens.write().unwrap_or_else(|p| p.into_inner());
    tokens.insert(user_id.to_string(), (token, Instant::now()));
    Ok(())
  }

  fn get_token(&self, user_id: &str) -> OAuthResult<Option<OAuthTokenResponse>> {
    let tokens = self.tokens.read().unwrap_or_else(|p| p.into_inner());
    Ok(tokens.get(user_id).map(|(token, _)| token.clone()))
  }

  fn remove_token(&self, user_id: &str) -> OAuthResult<()> {
    // PoisonError 是逻辑 bug（其他线程持锁 panic）而非 ConfigurationError；
    // recover via into_inner 让进程继续工作而非把内部状态映射为配置错误。
    let mut tokens = self.tokens.write().unwrap_or_else(|p| p.into_inner());
    tokens.remove(user_id);
    Ok(())
  }

  fn list_tokens(&self) -> OAuthResult<Vec<(String, OAuthTokenResponse)>> {
    let tokens = self.tokens.read().unwrap_or_else(|p| p.into_inner());
    Ok(tokens.iter().map(|(id, (token, _))| (id.clone(), token.clone())).collect())
  }

  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
}

pub struct OAuthProvider {
  pub name: String,
  pub auth_url: String,
  pub token_url: String,
  pub user_info_url: String,
  pub scopes: Vec<String>,
}

/// 按 provider 选择 user-info 请求的 `Authorization` header scheme。
///
/// GitHub / Gitee 使用非标准的 `token <access_token>` 形式；其余 provider
/// 走标准 OAuth 2.0 Bearer（RFC 6750）。
fn auth_scheme(provider_name: &str) -> &'static str {
  match provider_name {
    "github" | "gitee" => "token",
    _ => "Bearer",
  }
}

impl OAuthProvider {
  pub fn gitee() -> Self {
    Self {
      name: "gitee".to_string(),
      auth_url: "https://gitee.com/oauth/authorize".to_string(),
      token_url: "https://gitee.com/oauth/token".to_string(),
      user_info_url: "https://gitee.com/api/v5/user".to_string(),
      scopes: vec!["user_info".to_string()],
    }
  }

  pub fn github() -> Self {
    Self {
      name: "github".to_string(),
      auth_url: "https://github.com/login/oauth/authorize".to_string(),
      token_url: "https://github.com/login/oauth/access_token".to_string(),
      user_info_url: "https://api.github.com/user".to_string(),
      scopes: vec!["user:email".to_string()],
    }
  }

  pub fn build_client(
    &self,
    config: &OAuthConfig,
  ) -> OAuthResult<BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>> {
    let client_id = ClientId::new(config.client_id.clone());
    let client_secret = ClientSecret::new(config.client_secret.clone());

    let auth_url = AuthUrl::new(self.auth_url.clone()).map_err(|e| OAuthError::ConfigurationError(e.to_string()))?;

    let token_url = TokenUrl::new(self.token_url.clone()).map_err(|e| OAuthError::ConfigurationError(e.to_string()))?;

    let redirect_url =
      RedirectUrl::new(config.redirect_url.clone()).map_err(|e| OAuthError::ConfigurationError(e.to_string()))?;

    let client = BasicClient::new(client_id)
      .set_client_secret(client_secret)
      .set_auth_uri(auth_url)
      .set_token_uri(token_url)
      .set_redirect_uri(redirect_url);

    Ok(client)
  }

  pub async fn get_user_info(&self, access_token: &str) -> OAuthResult<UserInfo> {
    let client = shared_http_client();

    let response = client
      .get(&self.user_info_url)
      .header("Authorization", format!("{} {}", auth_scheme(&self.name), access_token))
      .header("User-Agent", "fusion-security-oauth/0.1.0") // GitHub requires User-Agent header
      .header("Accept", "application/vnd.github.v3+json") // Explicitly request GitHub API v3
      .send()
      .await
      .map_err(|e| OAuthError::RequestError(e.to_string()))?;

    if !response.status().is_success() {
      return Err(OAuthError::UserInfoError(format!(
        "HTTP {}: {}",
        response.status(),
        response.text().await.unwrap_or_default()
      )));
    }

    let user_data: HashMap<String, serde_json::Value> =
      response.json().await.map_err(|e| OAuthError::UserInfoError(e.to_string()))?;

    let user_info = self.parse_user_info(user_data)?;
    Ok(user_info)
  }

  /// 刷新访问令牌
  pub async fn refresh_access_token(
    &self,
    refresh_token: &str,
    config: &OAuthConfig,
  ) -> OAuthResult<OAuthTokenResponse> {
    let client = self.build_client(config)?;

    let refresh_token = RefreshToken::new(refresh_token.to_string());

    let token = client
      .exchange_refresh_token(&refresh_token)
      .request_async(&http_client)
      .await
      .map_err(|e| OAuthError::TokenExchangeError(e.to_string()))?;

    let token_response = OAuthTokenResponse {
      access_token: token.access_token().secret().clone(),
      token_type: format!("{:?}", token.token_type()),
      expires_in: token.expires_in().map(|d| d.as_secs()),
      refresh_token: token.refresh_token().map(|t| t.secret().clone()),
      scope: token.scopes().map(|s| s.iter().map(|scope| scope.to_string()).collect::<Vec<_>>().join(" ")),
    };

    Ok(token_response)
  }

  fn parse_user_info(&self, data: HashMap<String, serde_json::Value>) -> OAuthResult<UserInfo> {
    match self.name.as_str() {
      "gitee" => self.parse_gitee_user_info(data),
      "github" => self.parse_github_user_info(data),
      other => Err(OAuthError::UnsupportedProvider(other.to_string())),
    }
  }

  fn parse_gitee_user_info(&self, data: HashMap<String, serde_json::Value>) -> OAuthResult<UserInfo> {
    let id = data
      .get("id")
      .and_then(|v| v.as_i64())
      .ok_or_else(|| OAuthError::UserInfoError("Missing id field".to_string()))?
      .to_string();

    let username = data.get("login").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let name = data.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
    let email = data.get("email").and_then(|v| v.as_str()).map(|s| s.to_string());
    let avatar_url = data.get("avatar_url").and_then(|v| v.as_str()).map(|s| s.to_string());

    Ok(UserInfo { id, username, name, email, avatar_url, provider: "gitee".to_string() })
  }

  fn parse_github_user_info(&self, data: HashMap<String, serde_json::Value>) -> OAuthResult<UserInfo> {
    let id = data
      .get("id")
      .and_then(|v| v.as_i64())
      .ok_or_else(|| OAuthError::UserInfoError("Missing id field".to_string()))?
      .to_string();

    let username = data.get("login").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let name = data.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
    let email = data.get("email").and_then(|v| v.as_str()).map(|s| s.to_string());
    let avatar_url = data.get("avatar_url").and_then(|v| v.as_str()).map(|s| s.to_string());

    Ok(UserInfo { id, username, name, email, avatar_url, provider: "github".to_string() })
  }
}

pub struct OAuthClient {
  provider: OAuthProvider,
  client: BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>,
  config: OAuthConfig,
  token_store: Arc<dyn TokenStore>,
}

impl OAuthClient {
  /// 创建 OAuth 客户端，默认启用自动续期（使用内存存储）
  pub fn new(provider: OAuthProvider, config: &OAuthConfig) -> OAuthResult<Self> {
    let client = provider.build_client(config)?;
    Ok(Self { provider, client, config: config.clone(), token_store: Arc::new(MemoryTokenStore::new()) })
  }

  /// 创建带有自定义令牌存储的 OAuth 客户端
  pub fn with_token_store(
    provider: OAuthProvider,
    config: &OAuthConfig,
    token_store: Arc<dyn TokenStore>,
  ) -> OAuthResult<Self> {
    let client = provider.build_client(config)?;
    Ok(Self { provider, client, config: config.clone(), token_store })
  }

  /// 交换授权码并存储令牌。
  ///
  /// CSRF defence: `received_state` is the `state` query parameter from the
  /// provider's callback redirect; `expected_state` is the value the caller
  /// passed to [`OAuthClient::get_authorize_url`] and persisted (typically in
  /// the user session). They are compared in constant time before the code is
  /// exchanged — a mismatch aborts with [`OAuthError::StateMismatch`] so a
  /// forged callback cannot bind an attacker's authorization code to the
  /// victim's account.
  pub async fn exchange_code(
    &self,
    code: &str,
    received_state: &str,
    expected_state: &str,
    user_id: &str,
  ) -> OAuthResult<OAuthTokenResponse> {
    if !constant_time_eq(received_state.as_bytes(), expected_state.as_bytes()) {
      return Err(OAuthError::StateMismatch);
    }

    let token = self
      .client
      .exchange_code(AuthorizationCode::new(code.to_string()))
      .request_async(&http_client)
      .await
      .map_err(|e| OAuthError::TokenExchangeError(e.to_string()))?;

    let token_response = OAuthTokenResponse {
      access_token: token.access_token().secret().clone(),
      token_type: format!("{:?}", token.token_type()),
      expires_in: token.expires_in().map(|d| d.as_secs()),
      refresh_token: token.refresh_token().map(|t| t.secret().clone()),
      scope: token.scopes().map(|s| s.iter().map(|scope| scope.to_string()).collect::<Vec<_>>().join(" ")),
    };

    // 自动存储令牌
    self.token_store.store_token(user_id, token_response.clone())?;
    Ok(token_response)
  }

  /// 获取用户信息（自动处理令牌续期）
  pub async fn get_user_info(&self, user_id: &str) -> OAuthResult<UserInfo> {
    let access_token = self.get_valid_access_token(user_id).await?;
    self.provider.get_user_info(&access_token).await
  }

  /// 获取有效的访问令牌（内部方法，自动处理续期）
  async fn get_valid_access_token(&self, user_id: &str) -> OAuthResult<String> {
    // 从存储获取令牌
    if let Some(token) = self.token_store.get_token(user_id)? {
      // 检查令牌是否过期或即将过期
      if token.is_expired_or_expiring_soon() {
        // 尝试刷新令牌
        if let Some(refresh_token) = &token.refresh_token {
          match self.provider.refresh_access_token(refresh_token, &self.config).await {
            Ok(new_token) => {
              // 更新存储中的令牌
              self.token_store.store_token(user_id, new_token.clone())?;
              return Ok(new_token.access_token);
            }
            Err(_) => {
              // 刷新失败，移除过期的令牌
              self.token_store.remove_token(user_id)?;
              return Err(OAuthError::TokenExchangeError("Token refresh failed and token was removed".to_string()));
            }
          }
        } else {
          // 没有刷新令牌，移除过期的令牌
          self.token_store.remove_token(user_id)?;
          return Err(OAuthError::TokenExchangeError("Token expired and no refresh token available".to_string()));
        }
      }
      // 令牌仍然有效
      return Ok(token.access_token);
    }

    Err(OAuthError::TokenExchangeError(
      "No valid token found in storage. Please exchange authorization code first.".to_string(),
    ))
  }

  /// 获取令牌存储的引用（用于高级用法）
  pub fn token_store(&self) -> &Arc<dyn TokenStore> {
    &self.token_store
  }

  pub fn get_authorize_url(&self, state: &str) -> String {
    let scopes: Vec<Scope> = self.provider.scopes.iter().map(|s| Scope::new(s.clone())).collect();

    let (auth_url, _) = self.client.authorize_url(|| CsrfToken::new(state.to_string())).add_scopes(scopes).url();

    auth_url.to_string()
  }
}

#[cfg(test)]
mod tests {
  use super::constant_time_eq;

  #[test]
  fn constant_time_eq_matches_equal_slices() {
    assert!(constant_time_eq(b"random_state_abc123", b"random_state_abc123"));
    assert!(constant_time_eq(b"", b""));
  }

  #[test]
  fn constant_time_eq_rejects_different_content() {
    assert!(!constant_time_eq(b"random_state_abc123", b"random_state_abc124"));
    assert!(!constant_time_eq(b"attacker_state", b"victim_state__"));
  }

  #[test]
  fn constant_time_eq_rejects_different_length() {
    assert!(!constant_time_eq(b"short", b"short_with_suffix"));
    assert!(!constant_time_eq(b"", b"x"));
  }
}
