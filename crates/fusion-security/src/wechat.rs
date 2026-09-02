//! 微信开放平台 OAuth2 认证原语（feature `with-wechat`，r414 重建批）。
//!
//! 职责边界（owner 技术要求分层）：本模块 = 认证原语——HTTP 换码调用 + errcode
//! 映射（Invalid / Unavailable 两类）+ 会话字段解析；双凭据面编排与 unionid 强制
//! 断言在 fusion-weixin crate（消费方 chiying r414）。
//!
//! 端点：`GET {base}/sns/oauth2/access_token?appid&secret&code&grant_type=
//! authorization_code`——开放平台「移动应用」与「网站应用」同端点异凭据（凭据面
//! 选择由上层 channel 路由）。`endpoint_base` 可参数化（缺省官方
//! `https://api.weixin.qq.com`——mock / 代理测试通道）。
//!
//! 明文纪律：secret / code 只进 query 不落日志；错误日志仅打类别与 errcode。

use std::time::Duration;

use serde::Deserialize;

/// 官方端点 base（缺省）。
pub const DEFAULT_WECHAT_ENDPOINT_BASE: &str = "https://api.weixin.qq.com";

/// 微信认证错误（两类映射——消费方据此分流 unauthenticated / unavailable）。
#[derive(Debug, thiserror::Error)]
pub enum WechatAuthError {
  /// 换码被拒（errcode 非 0 且非系统忙——code 无效 / 凭据无效等请求侧错误）。
  #[error("wechat auth rejected: errcode={code} message={message}")]
  Invalid { code: i64, message: String },
  /// 出站依赖不可用（网络不可达 / 超时 / HTTP 层非 200 / 响应畸形 / 微信系统忙 -1）。
  #[error("wechat auth unavailable: {message}")]
  Unavailable { message: String },
}

impl From<reqwest::Error> for WechatAuthError {
  fn from(e: reqwest::Error) -> Self {
    Self::Unavailable { message: e.to_string() }
  }
}

/// 微信系统忙 errcode（官方文档：-1 时调用方应重试——按不可用降级而非凭据错误）。
const WECHAT_ERR_SYSTEM_BUSY: i64 = -1;

/// 换码成功会话（官方响应字段子集——只取有消费面的）。
#[derive(Debug, Clone)]
pub struct WechatCodeSession {
  pub access_token: String,
  pub openid: String,
  /// 应用未绑定开放平台时缺省（fusion-weixin 层强制断言——锚定主体必须 unionid）。
  pub unionid: Option<String>,
  /// 过期秒数（换码响应透传；刷新流未实现——演进位）。
  pub expires_in: Option<i64>,
}

/// 微信换码响应（errcode 缺省 = 0 成功——官方 JSON 形态）。
#[derive(Debug, Deserialize)]
struct CodeSessionResponse {
  #[serde(default)]
  access_token: String,
  #[serde(default)]
  openid: String,
  unionid: Option<String>,
  expires_in: Option<i64>,
  #[serde(default)]
  errcode: i64,
  #[serde(default)]
  errmsg: String,
}

/// 微信 OAuth2 换码客户端（认证原语——无状态，可复用）。
#[derive(Clone)]
pub struct WechatAuthClient {
  http: reqwest::Client,
  endpoint_base: String,
}

impl WechatAuthClient {
  /// 构造（`endpoint_base` 空 = 官方端点；timeout 施加于整个请求周期）。
  ///
  /// # Panics
  ///
  /// reqwest builder 仅在 TLS 后端初始化失败（环境级致命）时 Err——fail-fast
  /// 优于每请求降级。
  pub fn new(endpoint_base: &str, timeout: Duration) -> Self {
    let http = reqwest::Client::builder().timeout(timeout).build().expect("reqwest client build");
    let base = if endpoint_base.is_empty() { DEFAULT_WECHAT_ENDPOINT_BASE } else { endpoint_base };
    Self { http, endpoint_base: base.trim_end_matches('/').to_string() }
  }

  /// 授权码换会话（`grant_type=authorization_code` 固定——本模块唯一流程）。
  pub async fn code_to_session(
    &self,
    appid: &str,
    secret: &str,
    code: &str,
  ) -> Result<WechatCodeSession, WechatAuthError> {
    // query 参数经 url crate 构造（reqwest 0.13 query() 在未启用 feature 后——
    // workspace feature 面零扩张）
    let mut url = url::Url::parse(&format!("{}/sns/oauth2/access_token", self.endpoint_base))
      .map_err(|e| WechatAuthError::Unavailable { message: format!("invalid endpoint base: {e}") })?;
    url.query_pairs_mut().extend_pairs([
      ("appid", appid),
      ("secret", secret),
      ("code", code),
      ("grant_type", "authorization_code"),
    ]);
    let resp = self.http.get(url).send().await?;
    if !resp.status().is_success() {
      // secret / code 不进日志（明文纪律）
      tracing::warn!(status = %resp.status(), "wechat auth: endpoint http error");
      return Err(WechatAuthError::Unavailable { message: format!("http status {}", resp.status()) });
    }
    let body: CodeSessionResponse = resp
      .json()
      .await
      .map_err(|e| WechatAuthError::Unavailable { message: format!("malformed response body: {e}") })?;
    if body.errcode != 0 {
      if body.errcode == WECHAT_ERR_SYSTEM_BUSY {
        return Err(WechatAuthError::Unavailable { message: format!("errcode {} ({})", body.errcode, body.errmsg) });
      }
      return Err(WechatAuthError::Invalid { code: body.errcode, message: body.errmsg });
    }
    if body.access_token.is_empty() || body.openid.is_empty() {
      return Err(WechatAuthError::Unavailable { message: "success response missing access_token/openid".to_string() });
    }
    Ok(WechatCodeSession {
      access_token: body.access_token,
      openid: body.openid,
      unionid: body.unionid,
      expires_in: body.expires_in,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 极简 HTTP/1.1 mock 端点（TcpListener 手写响应——零新增 dev-dep）。
  /// 返回 (base_url, 响应体注入句柄按请求读取——每连接单请求语义)。
  async fn spawn_mock(responses: Vec<(u16, String)>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
      for (status, body) in responses {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _n = sock.read(&mut buf).await.unwrap();
        let resp = format!(
          "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
          body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
      }
    });
    format!("http://{addr}")
  }

  fn client(base: &str) -> WechatAuthClient {
    WechatAuthClient::new(base, Duration::from_secs(2))
  }

  #[tokio::test]
  async fn success_parses_session_fields() {
    let base =
      spawn_mock(vec![(200, r#"{"access_token":"at","openid":"oX","unionid":"oU","expires_in":7200}"#.to_string())])
        .await;
    let s = client(&base).code_to_session("appid", "secret", "code").await.unwrap();
    assert_eq!(s.openid, "oX");
    assert_eq!(s.unionid.as_deref(), Some("oU"));
    assert_eq!(s.expires_in, Some(7200));
  }

  #[tokio::test]
  async fn errcode_40029_invalid_code_maps_invalid() {
    let base = spawn_mock(vec![(200, r#"{"errcode":40029,"errmsg":"invalid code"}"#.to_string())]).await;
    let e = client(&base).code_to_session("appid", "secret", "bad").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Invalid { code: 40029, .. }));
  }

  #[tokio::test]
  async fn errcode_40013_invalid_appid_maps_invalid() {
    let base = spawn_mock(vec![(200, r#"{"errcode":40013,"errmsg":"invalid appid"}"#.to_string())]).await;
    let e = client(&base).code_to_session("bad-appid", "secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Invalid { code: 40013, .. }));
  }

  #[tokio::test]
  async fn errcode_40125_invalid_secret_maps_invalid() {
    let base = spawn_mock(vec![(200, r#"{"errcode":40125,"errmsg":"invalid appsecret"}"#.to_string())]).await;
    let e = client(&base).code_to_session("appid", "bad-secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Invalid { code: 40125, .. }));
  }

  #[tokio::test]
  async fn errcode_system_busy_maps_unavailable() {
    let base = spawn_mock(vec![(200, r#"{"errcode":-1,"errmsg":"system busy"}"#.to_string())]).await;
    let e = client(&base).code_to_session("appid", "secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn http_500_maps_unavailable() {
    let base = spawn_mock(vec![(500, r#"{"errcode":-1}"#.to_string())]).await;
    let e = client(&base).code_to_session("appid", "secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn malformed_body_maps_unavailable() {
    let base = spawn_mock(vec![(200, "not-json".to_string())]).await;
    let e = client(&base).code_to_session("appid", "secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn missing_core_fields_maps_unavailable() {
    // errcode=0 但 access_token / openid 缺失 = 上游异常形态，按不可用降级
    let base = spawn_mock(vec![(200, r#"{"openid":"oX"}"#.to_string())]).await;
    let e = client(&base).code_to_session("appid", "secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn endpoint_unreachable_maps_unavailable() {
    // 保留端口无监听 → 连接拒绝 → Unavailable（不误映射 Invalid）
    let e = client("http://127.0.0.1:1").code_to_session("appid", "secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[test]
  fn empty_endpoint_base_defaults_to_official() {
    let c = WechatAuthClient::new("", Duration::from_secs(1));
    assert_eq!(c.endpoint_base, DEFAULT_WECHAT_ENDPOINT_BASE);
    let c2 = WechatAuthClient::new("http://mock/", Duration::from_secs(1));
    assert_eq!(c2.endpoint_base, "http://mock");
  }
}
