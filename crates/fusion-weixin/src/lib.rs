//! fusion-weixin —— 微信登录编排（r414 重建批；小程序凭据面 g032 增补）。
//!
//! 职责分层（owner 技术要求）：认证原语（HTTP 换码 + errcode 映射）在
//! fusion-security `with-wechat` feature（`WechatAuthClient`——oauth2 与
//! jscode2session 两换码面）；本 crate = 多凭据面编排 + 锚定策略：
//! - **三凭据面**：开放平台「移动应用」（App channel——RN-iOS / OHOS SDK 授权流）
//!   与「网站应用」（Web channel——扫码 / 快速登录流）各持 appid/secret，同端点
//!   异凭据；**小程序面**（MiniProgram——`wx.login` code 经 jscode2session 换取，
//!   小程序自身 appid/secret）；
//! - **锚定策略（通道分化，g032 owner 裁决）**：App/Web 通道身份主体必须 unionid
//!   （同主体跨应用稳定；响应缺 unionid → `MissingUnionid` fail-closed，MUST NOT
//!   回退 openid——应用未绑定开放平台的配置缺陷不降级为弱锚定）；**小程序通道
//!   锚 openid、unionid 可选随附**——小程序可独立于微信开放平台存在，未绑定时
//!   unionid 结构性缺失是合法常态而非配置缺陷；
//! - **入口自检**（复查批 ①）：`exchange` / `exchange_mp` 对未配置通道直接
//!   `Unavailable`——防空凭据出站被微信 40013 误映射 `Invalid`（凭据错误与通道
//!   未启用是两类故障）。
//!
//! session_key 纪律：jscode2session 响应的 `session_key` 在 fusion-security 原语
//! 层解析即弃——本 crate 任何结构 / 日志 / 错误面均不出现该字段（本面无微信加密
//! 数据消费诉求；如有解密诉求须另立显式原语并重过隐私评审）。
//!
//! 明文纪律：secret / code 不落日志；错误日志仅打 channel 与错误类别。
//! 携密纪律：`WeixinCredentials` 手写脱敏 Debug（secret 字段 `<REDACTED>`），
//! MUST NOT derive(Debug)。

use std::fmt;
use std::time::Duration;

use fusion_security::wechat::{WechatAuthClient, WechatAuthError};

/// 单通道凭据（appid + secret 成对——半配在消费方装配期 fail-closed）。
///
/// 携密类型手写脱敏 Debug：appid（前端可见的公开标识）保留、secret 恒
/// `<REDACTED>`——MUST NOT derive(Debug)（日志 / 调试输出泄密面）。
#[derive(Clone)]
pub struct WeixinCredentials {
  appid: String,
  secret: String,
}

impl fmt::Debug for WeixinCredentials {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("WeixinCredentials")
      .field("appid", &self.appid)
      .field("secret", &"<REDACTED>")
      .finish()
  }
}

impl WeixinCredentials {
  pub fn new(appid: String, secret: String) -> Self {
    Self { appid, secret }
  }

  /// 全空（未启用）或全配（可用）才合法；半配由消费方启动期断言拒。
  pub fn is_configured(&self) -> bool {
    !self.appid.is_empty() && !self.secret.is_empty()
  }
}

/// 凭据面选择（开放平台应用类型——同端点异凭据）。小程序面不在此枚举（产出
/// 结构与锚定策略分立，走 [`WeixinLoginClient::exchange_mp`] 独立入口）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeixinChannel {
  /// 「移动应用」凭据面（RN-iOS / OHOS SDK 授权流）。
  App,
  /// 「网站应用」凭据面（Web 扫码 / 快速登录流）。
  Web,
}

/// oauth2 面换码产出（身份锚定主体 = unionid——强制非空）。
#[derive(Debug, Clone)]
pub struct WeixinToken {
  pub unionid: String,
  pub openid: String,
}

/// 小程序面换码产出（锚定策略通道分化：**锚 openid**，unionid 可选随附——
/// 小程序可独立于开放平台存在，未绑定时 unionid 缺失是合法常态，MUST NOT
/// 视为 [`WeixinError::MissingUnionid`] 类故障）。
#[derive(Debug, Clone)]
pub struct MpToken {
  pub openid: String,
  pub unionid: Option<String>,
}

/// 换码错误（消费方映射：Invalid / MissingUnionid → unauthenticated、Unavailable →
/// unavailable——两类分流稳定性契约）。
#[derive(Debug, thiserror::Error)]
pub enum WeixinError {
  /// 换码被拒（code 无效 / 凭据无效等请求侧错误——errcode 随附诊断）。
  #[error("weixin exchange rejected: errcode={code} message={message}")]
  Invalid { code: i64, message: String },
  /// 出站依赖不可用（通道未配置 / 网络不可达 / 超时 / 微信系统忙）。
  #[error("weixin exchange unavailable: {message}")]
  Unavailable { message: String },
  /// 响应缺 unionid（应用未绑定开放平台——fail-closed，MUST NOT 回退 openid）。
  #[error("weixin exchange succeeded but unionid is missing (app not bound to open platform)")]
  MissingUnionid,
}

impl From<WechatAuthError> for WeixinError {
  fn from(e: WechatAuthError) -> Self {
    match e {
      WechatAuthError::Invalid { code, message } => WeixinError::Invalid { code, message },
      WechatAuthError::Unavailable { message } => WeixinError::Unavailable { message },
    }
  }
}

/// 微信登录客户端（三凭据面编排——无状态，可复用）。
#[derive(Clone)]
pub struct WeixinLoginClient {
  app: WeixinCredentials,
  web: WeixinCredentials,
  /// 小程序凭据面（`None` = 既有双面构造形态未启用——兼容位，随 g032 增）。
  mp: Option<WeixinCredentials>,
  auth: WechatAuthClient,
  timeout: Duration,
}

impl WeixinLoginClient {
  /// 构造（`endpoint_base` 空 = 官方端点；timeout 施加于换码全程——双层：
  /// reqwest 请求级 + tokio future 级兜底）。小程序面未启用（既有双面形态，
  /// 签名不变——既有消费方零改动）。
  pub fn new(app: WeixinCredentials, web: WeixinCredentials, endpoint_base: &str, timeout: Duration) -> Self {
    Self { app, web, mp: None, auth: WechatAuthClient::new(endpoint_base, timeout), timeout }
  }

  /// 三面构造（g032 增——App / Web / MiniProgram 凭据齐配；小程序面 `None`
  /// 语义同未配置凭据 = `exchange_mp` 入口自检拒）。
  pub fn new_with_mp(
    app: WeixinCredentials,
    web: WeixinCredentials,
    mp: Option<WeixinCredentials>,
    endpoint_base: &str,
    timeout: Duration,
  ) -> Self {
    Self { app, web, mp, auth: WechatAuthClient::new(endpoint_base, timeout), timeout }
  }

  /// 通道凭据齐备性（分面装配——单面齐配即该面可用，未配面运行期降级）。
  pub fn is_configured(&self, channel: WeixinChannel) -> bool {
    match channel {
      WeixinChannel::App => self.app.is_configured(),
      WeixinChannel::Web => self.web.is_configured(),
    }
  }

  /// 小程序面凭据齐备性。
  pub fn is_configured_mp(&self) -> bool {
    self.mp.as_ref().is_some_and(|c| c.is_configured())
  }

  /// 授权码换身份（unionid 锚定）。未配置通道 → `Unavailable`（入口自检——防空
  /// 凭据出站被微信 40013 误映射 `Invalid`）；缺 unionid → `MissingUnionid`。
  pub async fn exchange(&self, channel: WeixinChannel, code: &str) -> Result<WeixinToken, WeixinError> {
    let cred = match channel {
      WeixinChannel::App => &self.app,
      WeixinChannel::Web => &self.web,
    };
    if !cred.is_configured() {
      return Err(WeixinError::Unavailable { message: format!("{channel:?} channel credentials not configured") });
    }
    let session = tokio::time::timeout(self.timeout, self.auth.code_to_session(&cred.appid, &cred.secret, code))
      .await
      .map_err(|_| WeixinError::Unavailable { message: "exchange timed out".to_string() })??;
    let Some(unionid) = session.unionid else {
      return Err(WeixinError::MissingUnionid);
    };
    Ok(WeixinToken { unionid, openid: session.openid })
  }

  /// 小程序 code 换身份（jscode2session——**锚定策略与 oauth2 面分立**：锚
  /// openid、unionid 可选随附，缺 unionid 是合法常态而非 `MissingUnionid` 故障）。
  /// 未配置小程序面 → `Unavailable`（入口自检，同 `exchange`）。
  pub async fn exchange_mp(&self, js_code: &str) -> Result<MpToken, WeixinError> {
    let Some(cred) = self.mp.as_ref().filter(|c| c.is_configured()) else {
      return Err(WeixinError::Unavailable { message: "MiniProgram channel credentials not configured".to_string() });
    };
    // session_key 由原语层解析即弃——本层不接触（MpToken 无承载面）
    let session = tokio::time::timeout(self.timeout, self.auth.jscode_to_session(&cred.appid, &cred.secret, js_code))
      .await
      .map_err(|_| WeixinError::Unavailable { message: "exchange timed out".to_string() })??;
    Ok(MpToken { openid: session.openid, unionid: session.unionid })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 极简 HTTP/1.1 mock 端点（TcpListener 手写响应——与 fusion-security wechat
  /// 测试同款形态；断言出站 query 携带对应凭据面的 appid）。
  async fn spawn_mock(status: u16, body: String) -> (String, std::sync::Arc<tokio::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let seen_cloned = seen.clone();
    tokio::spawn(async move {
      let (mut sock, _) = listener.accept().await.unwrap();
      let mut buf = [0u8; 4096];
      let n = sock.read(&mut buf).await.unwrap();
      let req = String::from_utf8_lossy(&buf[..n]).to_string();
      seen_cloned.lock().await.push(req);
      let resp = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
      );
      sock.write_all(resp.as_bytes()).await.unwrap();
    });
    (format!("http://{addr}"), seen)
  }

  fn client(base: &str) -> WeixinLoginClient {
    WeixinLoginClient::new(
      WeixinCredentials::new("wx-app".to_string(), "s-app".to_string()),
      WeixinCredentials::new("wx-web".to_string(), "s-web".to_string()),
      base,
      Duration::from_secs(2),
    )
  }

  #[tokio::test]
  async fn exchange_app_channel_returns_unionid_and_uses_app_credentials() {
    let (base, seen) = spawn_mock(200, r#"{"access_token":"at","openid":"oX","unionid":"oU"}"#.to_string()).await;
    let t = client(&base).exchange(WeixinChannel::App, "code-app").await.unwrap();
    assert_eq!(t.unionid, "oU");
    assert_eq!(t.openid, "oX");
    let req = seen.lock().await.remove(0);
    assert!(req.contains("appid=wx-app"), "app 面必须用 app 凭据: {req}");
  }

  #[tokio::test]
  async fn exchange_web_channel_uses_web_credentials() {
    let (base, seen) = spawn_mock(200, r#"{"access_token":"at","openid":"oX","unionid":"oU"}"#.to_string()).await;
    client(&base).exchange(WeixinChannel::Web, "code-web").await.unwrap();
    let req = seen.lock().await.remove(0);
    assert!(req.contains("appid=wx-web"), "web 面必须用 web 凭据: {req}");
  }

  #[tokio::test]
  async fn missing_unionid_fail_closed() {
    let (base, _) = spawn_mock(200, r#"{"access_token":"at","openid":"oX"}"#.to_string()).await;
    let e = client(&base).exchange(WeixinChannel::App, "code").await.unwrap_err();
    assert!(matches!(e, WeixinError::MissingUnionid));
  }

  #[tokio::test]
  async fn unconfigured_channel_entry_check_is_unavailable() {
    // 复查批 ①：未配置通道必须入口自检拒（Unavailable）——不出站（不会撞
    // 微信 40013 被误映射 Invalid）
    let c = WeixinLoginClient::new(
      WeixinCredentials::new("wx-app".to_string(), "s-app".to_string()),
      WeixinCredentials::new(String::new(), String::new()),
      "http://127.0.0.1:1",
      Duration::from_secs(1),
    );
    assert!(!c.is_configured(WeixinChannel::Web));
    let e = c.exchange(WeixinChannel::Web, "code").await.unwrap_err();
    assert!(matches!(e, WeixinError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn errcode_invalid_propagates_as_invalid() {
    let (base, _) = spawn_mock(200, r#"{"errcode":40029,"errmsg":"invalid code"}"#.to_string()).await;
    let e = client(&base).exchange(WeixinChannel::App, "bad").await.unwrap_err();
    assert!(matches!(e, WeixinError::Invalid { code: 40029, .. }));
  }

  #[tokio::test]
  async fn endpoint_unreachable_is_unavailable() {
    let e = client("http://127.0.0.1:1").exchange(WeixinChannel::App, "code").await.unwrap_err();
    assert!(matches!(e, WeixinError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn exchange_timeout_is_unavailable() {
    // 挂起不响应的端点 → tokio future 级兜底超时 → Unavailable
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
      // 接受连接但永不响应
      let _ = listener.accept().await;
      std::future::pending::<()>().await;
    });
    let e = WeixinLoginClient::new(
      WeixinCredentials::new("a".to_string(), "s".to_string()),
      WeixinCredentials::new(String::new(), String::new()),
      &format!("http://{addr}"),
      Duration::from_millis(200),
    )
    .exchange(WeixinChannel::App, "code")
    .await
    .unwrap_err();
    assert!(matches!(e, WeixinError::Unavailable { .. }));
  }

  #[test]
  fn credentials_half_configured_reports_not_configured() {
    assert!(WeixinCredentials::new("a".to_string(), "s".to_string()).is_configured());
    assert!(!WeixinCredentials::new("a".to_string(), String::new()).is_configured());
    assert!(!WeixinCredentials::new(String::new(), String::new()).is_configured());
  }

  #[test]
  fn credentials_debug_redacts_secret() {
    // 携密脱敏锚：Debug 输出 MUST NOT 含 secret 明文（appid 为前端可见公开标识保留）
    let cred = WeixinCredentials::new("wx-app".to_string(), "s3cret-value".to_string());
    let dbg = format!("{cred:?}");
    assert!(dbg.contains("<REDACTED>"), "secret must be redacted: {dbg}");
    assert!(!dbg.contains("s3cret-value"), "secret leak in Debug: {dbg}");
    assert!(dbg.contains("wx-app"));
  }

  // ---- 小程序面（jscode2session，g032 增补——锚定策略通道分化）----

  fn mp_client(base: &str) -> WeixinLoginClient {
    WeixinLoginClient::new_with_mp(
      WeixinCredentials::new("wx-app".to_string(), "s-app".to_string()),
      WeixinCredentials::new("wx-web".to_string(), "s-web".to_string()),
      Some(WeixinCredentials::new("wx-mp".to_string(), "s-mp".to_string())),
      base,
      Duration::from_secs(2),
    )
  }

  #[tokio::test]
  async fn exchange_mp_returns_openid_anchor_with_optional_unionid() {
    let (base, seen) = spawn_mock(200, r#"{"openid":"oMP","session_key":"sk","unionid":"oU"}"#.to_string()).await;
    let t = mp_client(&base).exchange_mp("js-code").await.unwrap();
    assert_eq!(t.openid, "oMP");
    assert_eq!(t.unionid.as_deref(), Some("oU"));
    let req = seen.lock().await.remove(0);
    assert!(req.contains("/sns/jscode2session?"), "must hit jscode2session: {req}");
    assert!(req.contains("appid=wx-mp"), "mp 面必须用 mp 凭据: {req}");
    assert!(!req.contains("s-web"), "must not leak web credentials: {req}");
  }

  #[tokio::test]
  async fn exchange_mp_without_unionid_is_legal_norm() {
    // D7 通道分化锚：小程序未绑定开放平台 = unionid 缺失是合法常态（锚 openid）
    let (base, _) = spawn_mock(200, r#"{"openid":"oMP","session_key":"sk"}"#.to_string()).await;
    let t = mp_client(&base).exchange_mp("js-code").await.unwrap();
    assert_eq!(t.openid, "oMP");
    assert_eq!(t.unionid, None);
  }

  #[tokio::test]
  async fn exchange_mp_unconfigured_entry_check_is_unavailable() {
    // 既有双面构造（new）形态：mp = None → 入口自检拒（不出站）
    let c = WeixinLoginClient::new(
      WeixinCredentials::new("wx-app".to_string(), "s-app".to_string()),
      WeixinCredentials::new("wx-web".to_string(), "s-web".to_string()),
      "http://127.0.0.1:1",
      Duration::from_secs(1),
    );
    assert!(!c.is_configured_mp());
    let e = c.exchange_mp("js-code").await.unwrap_err();
    assert!(matches!(e, WeixinError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn exchange_mp_half_configured_reports_not_configured() {
    let c = WeixinLoginClient::new_with_mp(
      WeixinCredentials::new("a".to_string(), "s".to_string()),
      WeixinCredentials::new(String::new(), String::new()),
      Some(WeixinCredentials::new("wx-mp".to_string(), String::new())),
      "http://127.0.0.1:1",
      Duration::from_secs(1),
    );
    assert!(!c.is_configured_mp());
    let e = c.exchange_mp("js-code").await.unwrap_err();
    assert!(matches!(e, WeixinError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn exchange_mp_errcode_40029_propagates_invalid() {
    let (base, _) = spawn_mock(200, r#"{"errcode":40029,"errmsg":"invalid js_code"}"#.to_string()).await;
    let e = mp_client(&base).exchange_mp("bad").await.unwrap_err();
    assert!(matches!(e, WeixinError::Invalid { code: 40029, .. }));
  }

  #[tokio::test]
  async fn exchange_mp_errcode_45011_maps_unavailable() {
    let (base, _) = spawn_mock(200, r#"{"errcode":45011,"errmsg":"api minute-quota"}"#.to_string()).await;
    let e = mp_client(&base).exchange_mp("js-code").await.unwrap_err();
    assert!(matches!(e, WeixinError::Unavailable { .. }));
  }

  #[tokio::test]
  async fn mp_token_debug_has_no_session_key_surface() {
    // session_key 零落点锚：编排层产出结构与 Debug 输出无该字段（编译期字段面 +
    // 运行期 Debug 双锚）
    let t = MpToken { openid: "oMP".to_string(), unionid: None };
    let dbg = format!("{t:?}");
    assert!(!dbg.contains("session_key"));
  }
}
