//! fusion-weixin —— 微信开放平台登录编排（r414 重建批）。
//!
//! 职责分层（owner 技术要求）：认证原语（HTTP 换码 + errcode 映射）在
//! fusion-security `with-wechat` feature（`WechatAuthClient`）；本 crate = 双凭据面
//! 编排 + unionid 强制断言：
//! - **双凭据面**：开放平台「移动应用」（App channel——RN-iOS / OHOS SDK 授权流）与
//!   「网站应用」（Web channel——扫码 / 快速登录流）各持 appid/secret，同端点异凭据；
//! - **unionid 锚定**：身份主体必须 unionid（同主体跨应用稳定）；响应缺 unionid →
//!   `MissingUnionid`（fail-closed，MUST NOT 回退 openid——应用未绑定开放平台的
//!   配置缺陷不降级为弱锚定）；
//! - **入口自检**（复查批 ①）：`exchange` 对未配置通道直接 `Unavailable`——防空
//!   凭据出站被微信 40013 误映射 `Invalid`（凭据错误与通道未启用是两类故障）。
//!
//! 明文纪律：secret / code 不落日志；错误日志仅打 channel 与错误类别。

use std::time::Duration;

use fusion_security::wechat::{WechatAuthClient, WechatAuthError};

/// 单通道凭据（appid + secret 成对——半配在消费方装配期 fail-closed）。
#[derive(Debug, Clone)]
pub struct WeixinCredentials {
  appid: String,
  secret: String,
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

/// 凭据面选择（开放平台应用类型——同端点异凭据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeixinChannel {
  /// 「移动应用」凭据面（RN-iOS / OHOS SDK 授权流）。
  App,
  /// 「网站应用」凭据面（Web 扫码 / 快速登录流）。
  Web,
}

/// 换码产出（身份锚定主体 = unionid——强制非空）。
#[derive(Debug, Clone)]
pub struct WeixinToken {
  pub unionid: String,
  pub openid: String,
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

/// 微信登录客户端（双凭据面编排——无状态，可复用）。
#[derive(Clone)]
pub struct WeixinLoginClient {
  app: WeixinCredentials,
  web: WeixinCredentials,
  auth: WechatAuthClient,
  timeout: Duration,
}

impl WeixinLoginClient {
  /// 构造（`endpoint_base` 空 = 官方端点；timeout 施加于换码全程——双层：
  /// reqwest 请求级 + tokio future 级兜底）。
  pub fn new(app: WeixinCredentials, web: WeixinCredentials, endpoint_base: &str, timeout: Duration) -> Self {
    Self { app, web, auth: WechatAuthClient::new(endpoint_base, timeout), timeout }
  }

  /// 通道凭据齐备性（分面装配——单面齐配即该面可用，未配面运行期降级）。
  pub fn is_configured(&self, channel: WeixinChannel) -> bool {
    match channel {
      WeixinChannel::App => self.app.is_configured(),
      WeixinChannel::Web => self.web.is_configured(),
    }
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
}
