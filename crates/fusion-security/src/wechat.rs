//! 微信开放平台认证原语（feature `with-wechat`，r414 重建批；jscode2session
//! 增补）。
//!
//! 职责边界（owner 技术要求分层）：本模块 = 认证原语——HTTP 换码调用 + errcode
//! 映射（Invalid / Unavailable 两类）+ 会话字段解析；多凭据面编排与锚定策略
//! 断言在 fusion-weixin crate。
//!
//! 两个换码面（同 base 异端点，响应结构分立 MUST NOT 复用混构）：
//! - **OAuth2 面**：`GET {base}/sns/oauth2/access_token?appid&secret&code&grant_type=
//!   authorization_code`——开放平台「移动应用」与「网站应用」同端点异凭据（凭据面
//!   选择由上层 channel 路由）；
//! - **小程序面**：`GET {base}/sns/jscode2session?appid&secret&js_code&grant_type=
//!   authorization_code`——微信小程序 `wx.login` code 换取（openid 恒有；unionid
//!   仅小程序绑定开放平台时返回——结构性可选）。
//!
//! `endpoint_base` 可参数化（缺省官方 `https://api.weixin.qq.com`——mock / 代理
//! 测试通道）。
//!
//! 明文纪律：secret / code 只进 query 不落日志；错误日志仅打类别与 errcode；
//! 小程序面 `session_key` 原语层解析即弃——不入任何返回值 / 日志 / 持久面
//! （无微信加密数据消费诉求）。

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
/// API 分钟配额 errcode（45011——服务侧限流，与系统忙同族按不可用降级；请求侧
/// 无可修复性，MUST NOT 映射凭据错误）。
const WECHAT_ERR_API_MINUTE_QUOTA: i64 = 45011;

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

/// 小程序 code2Session 会话（与 oauth2 面 [`WechatCodeSession`] 分立的独立
/// 结构——jscode2session 响应无 access_token / expires_in，MUST NOT 复用混构）。
///
/// `session_key` 纪律：微信返回的该字段在原语层**解析即弃**——不入本结构、
/// 不入日志、不向任何上层透出（本模块无微信加密数据消费诉求；消费方如有
/// 解密诉求须另立显式原语并重过隐私评审，MUST NOT 借道本结构）。
#[derive(Debug, Clone)]
pub struct MpSession {
  /// 小程序内用户唯一标识（官方口径恒有——锚定主体，见 fusion-weixin 锚定策略）。
  pub openid: String,
  /// 仅小程序已绑定微信开放平台账号时返回（结构性可选——未绑定 = 合法常态）。
  pub unionid: Option<String>,
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

/// 小程序 jscode2session 响应（与 oauth2 面 [`CodeSessionResponse`] 分立——无
/// access_token / expires_in；`session_key` 反序列化后即被丢弃，不进任何产出）。
#[derive(Debug, Deserialize)]
struct JsCodeSessionResponse {
  #[serde(default)]
  openid: String,
  unionid: Option<String>,
  /// 仅为满足反序列化形态——字段在构造 [`MpSession`] 前被丢弃（解析即弃纪律；
  /// 零读取是有意设计，非遗漏）。
  #[serde(default)]
  #[allow(dead_code)]
  session_key: String,
  #[serde(default)]
  errcode: i64,
  #[serde(default)]
  errmsg: String,
}

/// 微信 OAuth2 换码客户端（认证原语——无状态，可复用；oauth2 与小程序两换码面）。
#[derive(Clone)]
pub struct WechatAuthClient {
  http: reqwest::Client,
  endpoint_base: String,
}

/// errcode → 错误类别判定（两换码面共用：服务侧不可用族 = 系统忙 -1 / API 分钟
/// 配额 45011 → Unavailable；其余非零 = Invalid）。
///
/// 45011 改道为行为变更（此前仅特判 -1、45011 落 Invalid）——两换码面同效；
/// 既有 oauth2 用例未覆盖 45011，零改动回归成立。
fn classify_errcode(errcode: i64, errmsg: &str) -> WechatAuthError {
  if errcode == WECHAT_ERR_SYSTEM_BUSY || errcode == WECHAT_ERR_API_MINUTE_QUOTA {
    return WechatAuthError::Unavailable { message: format!("errcode {errcode} ({errmsg})") };
  }
  WechatAuthError::Invalid { code: errcode, message: errmsg.to_string() }
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
      return Err(classify_errcode(body.errcode, &body.errmsg));
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

  /// 小程序 code 换会话（`GET /sns/jscode2session`，`grant_type=authorization_code`
  /// 固定——与 oauth2 面同构异端点）。
  ///
  /// `session_key` 解析即弃（见 [`MpSession`] 纪律注）；openid 恒有性由官方契约
  /// 承诺，缺失按不可用降级（畸形响应）。
  pub async fn jscode_to_session(
    &self,
    appid: &str,
    secret: &str,
    js_code: &str,
  ) -> Result<MpSession, WechatAuthError> {
    let mut url = url::Url::parse(&format!("{}/sns/jscode2session", self.endpoint_base))
      .map_err(|e| WechatAuthError::Unavailable { message: format!("invalid endpoint base: {e}") })?;
    url.query_pairs_mut().extend_pairs([
      ("appid", appid),
      ("secret", secret),
      ("js_code", js_code),
      ("grant_type", "authorization_code"),
    ]);
    let resp = self.http.get(url).send().await?;
    if !resp.status().is_success() {
      // secret / js_code 不进日志（明文纪律）
      tracing::warn!(status = %resp.status(), "wechat auth: jscode2session http error");
      return Err(WechatAuthError::Unavailable { message: format!("http status {}", resp.status()) });
    }
    let body: JsCodeSessionResponse = resp
      .json()
      .await
      .map_err(|e| WechatAuthError::Unavailable { message: format!("malformed response body: {e}") })?;
    if body.errcode != 0 {
      return Err(classify_errcode(body.errcode, &body.errmsg));
    }
    if body.openid.is_empty() {
      return Err(WechatAuthError::Unavailable { message: "success response missing openid".to_string() });
    }
    // session_key 解析即弃——不进返回值（MpSession 纪律）
    Ok(MpSession { openid: body.openid, unionid: body.unionid })
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

  /// mock 端点（请求捕获形态——与 fusion-weixin 测试同款；断言出站路径 / 参数）。
  async fn spawn_mock_capture(
    responses: Vec<(u16, String)>,
  ) -> (String, std::sync::Arc<tokio::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let seen_cloned = seen.clone();
    tokio::spawn(async move {
      for (status, body) in responses {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).await.unwrap();
        seen_cloned.lock().await.push(String::from_utf8_lossy(&buf[..n]).to_string());
        let resp = format!(
          "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
          body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
      }
    });
    (format!("http://{addr}"), seen)
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

  // ---- 小程序面（jscode2session，g032 增补）----

  #[tokio::test]
  async fn jscode_success_with_unionid() {
    let base = spawn_mock(vec![(200, r#"{"openid":"oMP","session_key":"sk","unionid":"oU"}"#.to_string())]).await;
    let s = client(&base).jscode_to_session("appid", "secret", "js-code").await.unwrap();
    assert_eq!(s.openid, "oMP");
    assert_eq!(s.unionid.as_deref(), Some("oU"));
    // session_key 解析即弃——产出结构无该字段（编译期保证，MpSession 无字段即断言）
  }

  #[tokio::test]
  async fn jscode_success_without_unionid_is_normal() {
    // 小程序未绑定开放平台 = 合法常态（与 oauth2 面 MissingUnionid 语义分立）
    let base = spawn_mock(vec![(200, r#"{"openid":"oMP","session_key":"sk"}"#.to_string())]).await;
    let s = client(&base).jscode_to_session("appid", "secret", "js-code").await.unwrap();
    assert_eq!(s.openid, "oMP");
    assert_eq!(s.unionid, None);
  }

  #[tokio::test]
  async fn jscode_errcode_40029_invalid_code_maps_invalid() {
    let base = spawn_mock(vec![(200, r#"{"errcode":40029,"errmsg":"invalid js_code"}"#.to_string())]).await;
    let e = client(&base).jscode_to_session("appid", "secret", "bad").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Invalid { code: 40029, .. }));
  }

  #[tokio::test]
  async fn jscode_errcode_45011_minute_quota_maps_unavailable() {
    // 行为变更锚：45011 改道 Unavailable（服务侧限流族）——此前形态落 Invalid
    let base =
      spawn_mock(vec![(200, r#"{"errcode":45011,"errmsg":"api minute-quota reach limit"}"#.to_string())]).await;
    let e = client(&base).jscode_to_session("appid", "secret", "js-code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn jscode_errcode_40226_high_risk_maps_invalid() {
    let base = spawn_mock(vec![(200, r#"{"errcode":40226,"errmsg":"high risk user blocked"}"#.to_string())]).await;
    let e = client(&base).jscode_to_session("appid", "secret", "js-code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Invalid { code: 40226, .. }));
  }

  #[tokio::test]
  async fn jscode_errcode_system_busy_maps_unavailable() {
    let base = spawn_mock(vec![(200, r#"{"errcode":-1,"errmsg":"system busy"}"#.to_string())]).await;
    let e = client(&base).jscode_to_session("appid", "secret", "js-code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn jscode_45011_also_applies_to_oauth2_face() {
    // 45011 改道对既有 oauth2 面同效（行为变更回归锚）
    let base =
      spawn_mock(vec![(200, r#"{"errcode":45011,"errmsg":"api minute-quota reach limit"}"#.to_string())]).await;
    let e = client(&base).code_to_session("appid", "secret", "code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn jscode_endpoint_uses_jscode2session_path() {
    // 出站路径锚：/sns/jscode2session（非 oauth2 access_token 端点）
    let (base, seen) = spawn_mock_capture(vec![(200, r#"{"openid":"oMP"}"#.to_string())]).await;
    client(&base).jscode_to_session("appid", "secret", "jc").await.unwrap();
    let req = seen.lock().await.remove(0);
    assert!(req.contains("/sns/jscode2session?"), "must hit jscode2session endpoint: {req}");
    assert!(req.contains("js_code=jc"), "js_code param: {req}");
  }

  #[tokio::test]
  async fn jscode_missing_openid_maps_unavailable() {
    let base = spawn_mock(vec![(200, r#"{"session_key":"sk"}"#.to_string())]).await;
    let e = client(&base).jscode_to_session("appid", "secret", "js-code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn jscode_endpoint_unreachable_maps_unavailable() {
    let e = client("http://127.0.0.1:1").jscode_to_session("appid", "secret", "js-code").await.unwrap_err();
    assert!(matches!(e, WechatAuthError::Unavailable { .. }));
  }
}
