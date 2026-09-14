//! xpay（小程序虚拟支付）协议原语（p053 D4 分层——业务无关、可复用）：
//! 双签名（paySig / signature）、stable access_token 管理、xpay 服务端
//! 调用族（query_order 对账面；退款 / 投诉管理最小面同构可扩）。
//!
//! 签名口径（官方[虚拟支付签名](https://developers.weixin.qq.com/minigame/dev/guide/open-ability/virtual-payment/signature.html)
//! 锚定，单测含官方示例向量）：
//! - 客户端拉起：`paySig = hex(HMAC-SHA256(appKey, "requestVirtualPayment&" + signData))`；
//! - 服务端 API：`paySig = hex(HMAC-SHA256(appKey, uri + "&" + post_body))`；
//! - 用户态：`signature = hex(HMAC-SHA256(sessionKey, signData))`；
//! - signData 为**键名字典序**紧凑 JSON（验签端按字典序重组，字段顺序敏感），
//!   offerId 必须是字符串型。
//!
//! 明文纪律：appKey / session_key 不落日志；错误两分类（Invalid / Unavailable）
//! 沿 crate 既有纪律。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

/// HMAC-SHA256 → 十六进制（双签名与锚定共用口径）。
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
  let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC key accepts any length");
  Mac::update(&mut mac, message);
  hex::encode(mac.finalize().into_bytes())
}

/// signData 负载（道具直购八字段，键名字典序序列化——客户端必须原样透传，
/// MUST NOT 重新序列化）。
#[derive(Debug, Clone)]
pub struct SignDataParams<'a> {
  pub offer_id: &'a str,
  pub env: i32,
  pub product_id: &'a str,
  pub goods_price_cents: i32,
  pub out_trade_no: &'a str,
  pub buy_quantity: i32,
  /// 透传数据（必填，发货时原样透传）。
  pub attach: &'a str,
}

/// 构造 signData JSON 串（键名字典序：attach < buyQuantity < currencyType <
/// env < goodsPrice < offerId < outTradeNo < productId）。
pub fn build_sign_data(params: &SignDataParams<'_>) -> String {
  let quantity = params.buy_quantity.max(1);
  let price = params.goods_price_cents;
  // 手工拼字节串（不经 serde_json::Map——其键序非字典序保证面）：
  // 全字段无引号转义需求面（字母数字与固定串），attach 经 JSON 转义。
  let attach = params.attach.replace('\\', "\\\\").replace('"', "\\\"");
  format!(
    "{{\"attach\":\"{attach}\",\"buyQuantity\":{quantity},\"currencyType\":\"CNY\",\
     \"env\":{env},\"goodsPrice\":{price},\"offerId\":\"{offer_id}\",\
     \"outTradeNo\":\"{out_trade_no}\",\"productId\":\"{product_id}\"}}",
    env = params.env,
    offer_id = params.offer_id,
    out_trade_no = params.out_trade_no,
    product_id = params.product_id,
  )
}

/// 客户端拉起签名：paySig = HMAC-SHA256(AppKey, "requestVirtualPayment&" + signData)。
pub fn pay_sig_for_client(app_key: &str, sign_data: &str) -> String {
  hmac_sha256_hex(app_key.as_bytes(), format!("requestVirtualPayment&{sign_data}").as_bytes())
}

/// 服务端 API 签名：paySig = HMAC-SHA256(AppKey, uri + "&" + post_body)
///（uri 不带 query string）。
pub fn pay_sig_for_api(app_key: &str, uri: &str, post_body: &str) -> String {
  hmac_sha256_hex(app_key.as_bytes(), format!("{uri}&{post_body}").as_bytes())
}

/// 用户态签名：signature = HMAC-SHA256(sessionKey, signData)。
pub fn signature_with_session_key(session_key: &str, sign_data: &str) -> String {
  hmac_sha256_hex(session_key.as_bytes(), sign_data.as_bytes())
}

// ---------------------------------------------------------------------------
// stable access_token（xpay 服务端调用族凭证）
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum XpayError {
  /// 请求侧错误（errcode 非 0 且非系统忙——签名错 / 参数错 / 订单不存在等，
  /// errcode 随附诊断）。
  #[error("xpay rejected: errcode={code} message={message}")]
  Invalid { code: i64, message: String },
  /// 出站依赖不可用（网络 / 超时 / HTTP 非 2xx / 响应畸形 / 微信系统忙 -1）。
  #[error("xpay unavailable: {message}")]
  Unavailable { message: String },
}

impl From<reqwest::Error> for XpayError {
  fn from(e: reqwest::Error) -> Self {
    XpayError::Unavailable { message: e.to_string() }
  }
}

const XPAY_ERR_SYSTEM_BUSY: i64 = -1;

/// stable_token 响应（/cgi-bin/stable_token）。
#[derive(Debug, Deserialize)]
struct StableTokenResponse {
  #[serde(default)]
  access_token: String,
  #[serde(default)]
  expires_in: i64,
  #[serde(default)]
  errcode: i64,
  #[serde(default)]
  errmsg: String,
}

/// 进程内 stable_token 缓存（单实例现状；多实例时各进程独立获取——官方口径，
/// 刷新余量 300s 对齐第三方实战记录）。
struct StableTokenState {
  token: String,
  acquired_at: Instant,
  expires_in: Duration,
}

/// stable access_token 管理（grant_type=client_credential，appid+secret 换
/// 长期令牌；过期前 300s 提前刷新）。
#[derive(Clone)]
pub struct StableTokenManager {
  http: reqwest::Client,
  endpoint_base: String,
  appid: String,
  secret: String,
  state: std::sync::Arc<Mutex<Option<StableTokenState>>>,
  /// 提前刷新余量。
  refresh_margin: Duration,
}

impl StableTokenManager {
  /// 构造（内部自建 reqwest client——消费方零直连 reqwest 依赖面）。
  pub fn new(endpoint_base: &str, appid: String, secret: String, timeout: Duration) -> Self {
    let http = reqwest::Client::builder().timeout(timeout).build().expect("reqwest client build");
    let base =
      if endpoint_base.is_empty() { fusion_security::wechat::DEFAULT_WECHAT_ENDPOINT_BASE } else { endpoint_base };
    Self {
      http,
      endpoint_base: base.trim_end_matches('/').to_string(),
      appid,
      secret,
      state: std::sync::Arc::new(Mutex::new(None)),
      refresh_margin: Duration::from_secs(300),
    }
  }

  /// 取有效令牌（缓存命中且未近过期则直返；否则现场刷新）。秘密不落日志。
  pub async fn token(&self) -> Result<String, XpayError> {
    let cached = self.state.lock().expect("stable token lock").as_ref().and_then(|state| {
      (state.acquired_at + state.expires_in.saturating_sub(self.refresh_margin) > Instant::now())
        .then(|| state.token.clone())
    });
    match cached {
      Some(token) => Ok(token),
      None => self.refresh().await,
    }
  }

  async fn refresh(&self) -> Result<String, XpayError> {
    let body = serde_json::json!({
      "grant_type": "client_credential",
      "appid": self.appid,
      "secret": self.secret,
    });
    let resp = self.http.post(format!("{}/cgi-bin/stable_token", self.endpoint_base)).json(&body).send().await?;
    if !resp.status().is_success() {
      tracing::warn!(status = %resp.status(), "xpay stable_token http error");
      return Err(XpayError::Unavailable { message: format!("http status {}", resp.status()) });
    }
    let body: StableTokenResponse = resp
      .json()
      .await
      .map_err(|e| XpayError::Unavailable { message: format!("malformed response body: {e}") })?;
    if body.errcode != 0 {
      return Err(classify_xpay_errcode(body.errcode, &body.errmsg));
    }
    if body.access_token.is_empty() {
      return Err(XpayError::Unavailable { message: "stable_token success missing access_token".to_string() });
    }
    let expires_in = Duration::from_secs(body.expires_in.max(300) as u64);
    *self.state.lock().expect("stable token lock") =
      Some(StableTokenState { token: body.access_token.clone(), acquired_at: Instant::now(), expires_in });
    Ok(body.access_token)
  }
}

fn classify_xpay_errcode(errcode: i64, errmsg: &str) -> XpayError {
  if errcode == XPAY_ERR_SYSTEM_BUSY {
    return XpayError::Unavailable { message: format!("errcode {errcode} ({errmsg})") };
  }
  XpayError::Invalid { code: errcode, message: errmsg.to_string() }
}

// ---------------------------------------------------------------------------
// query_order（对账 / 兜底发货面；退款 / 投诉管理最小面同构可扩）
// ---------------------------------------------------------------------------

/// 订单定位（out_trade_no 与 wx_order_id 二选一）。
#[derive(Debug, Clone, Copy)]
pub enum OrderRef<'a> {
  OutTradeNo(&'a str),
  WxOrderId(&'a str),
}

/// query_order 返回的订单信息（官方字段子集 + 透传面；沙箱联调核对口径）。
#[derive(Debug, Clone, Deserialize)]
pub struct QueriedOrder {
  pub order_id: Option<String>,
  pub status: Option<i64>,
  pub order_fee: Option<i64>,
  pub paid_fee: Option<i64>,
  pub refund_fee: Option<i64>,
  pub order_type: Option<i64>,
  pub paid_time: Option<i64>,
  pub provide_time: Option<i64>,
  pub wx_order_id: Option<String>,
  pub env_type: Option<i64>,
}

impl QueriedOrder {
  /// 已支付待发货 / 发货中 / 已发货 = 支付成功面（status 2 / 3 / 4）。
  pub fn is_paid(&self) -> bool {
    matches!(self.status, Some(2..=4))
  }

  /// 退款面：status 5（已经退款）/ 8（用户退款完成）或 order_type 8（苹果退款）。
  pub fn is_refunded(&self) -> bool {
    matches!(self.status, Some(5 | 8)) || self.order_type == Some(8)
  }

  /// 关闭面：status 6（订单关闭不可再用）。
  pub fn is_closed(&self) -> bool {
    matches!(self.status, Some(6))
  }
}

#[derive(Debug, Deserialize)]
struct QueryOrderResponse {
  #[serde(default)]
  errcode: i64,
  #[serde(default)]
  errmsg: String,
  order: Option<QueriedOrder>,
}

/// xpay 服务端客户端（无状态，可复用；签名与传输在此层，业务语义归消费方）。
#[derive(Clone)]
pub struct XpayClient {
  http: reqwest::Client,
  endpoint_base: String,
  app_key: String,
  timeout: Duration,
}

impl XpayClient {
  pub fn new(endpoint_base: &str, app_key: String, timeout: Duration) -> Self {
    let http = reqwest::Client::builder().timeout(timeout).build().expect("reqwest client build");
    let base =
      if endpoint_base.is_empty() { fusion_security::wechat::DEFAULT_WECHAT_ENDPOINT_BASE } else { endpoint_base };
    Self { http, endpoint_base: base.trim_end_matches('/').to_string(), app_key, timeout }
  }

  pub fn timeout(&self) -> Duration {
    self.timeout
  }

  /// 查询创建的订单（现金单）。请求体 {openid, env, order_id | wx_order_id}；
  /// access_token + pay_sig 走 query string（官方口径）。
  pub async fn query_order(
    &self,
    access_token: &str,
    openid: &str,
    env: i32,
    order: OrderRef<'_>,
  ) -> Result<Option<QueriedOrder>, XpayError> {
    let mut body = serde_json::json!({ "openid": openid, "env": env });
    match order {
      OrderRef::OutTradeNo(out_trade_no) => body["order_id"] = serde_json::json!(out_trade_no),
      OrderRef::WxOrderId(wx_order_id) => body["wx_order_id"] = serde_json::json!(wx_order_id),
    }
    let post_body = body.to_string();
    let uri = "/xpay/query_order";
    let pay_sig = pay_sig_for_api(&self.app_key, uri, &post_body);
    let mut url = url::Url::parse(&format!("{}{uri}", self.endpoint_base))
      .map_err(|e| XpayError::Unavailable { message: format!("invalid endpoint base: {e}") })?;
    url.query_pairs_mut().extend_pairs([("access_token", access_token), ("pay_sig", pay_sig.as_str())]);
    let resp = self.http.post(url).header("Content-Type", "application/json").body(post_body).send().await?;
    if !resp.status().is_success() {
      tracing::warn!(status = %resp.status(), "xpay query_order http error");
      return Err(XpayError::Unavailable { message: format!("http status {}", resp.status()) });
    }
    let body: QueryOrderResponse = resp
      .json()
      .await
      .map_err(|e| XpayError::Unavailable { message: format!("malformed response body: {e}") })?;
    if body.errcode != 0 {
      return Err(classify_xpay_errcode(body.errcode, &body.errmsg));
    }
    Ok(body.order)
  }
}

// 测试辅助：从 JSON 串提键序列（字典序断言用）——键后须紧跟 ':'，区别于字符串值。
#[cfg(test)]
fn json_keys(src: &str) -> Vec<&str> {
  let mut keys = Vec::new();
  let mut rest = src;
  while let Some(start) = rest.find('"') {
    let after = &rest[start + 1..];
    let Some(end) = after.find('"') else { break };
    let (key, tail) = after.split_at(end);
    if tail[1..].starts_with(':') {
      keys.push(key);
    }
    rest = &tail[1..];
  }
  keys
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pay_sig_matches_official_vector() {
    // 官方示例向量（minigame 签名文档）：uri=/wxa/game/getbalance、appkey=12345、
    // post_body 固定串 → 11bac638...（服务端 API 签名公式 uri + '&' + body）。
    let post_body =
      r#"{"offer_id": "12345678", "openid": "oUrsfxxxxxxxxxx", "ts": 1668136271, "zone_id": "1", "env": 0}"#;
    assert_eq!(
      pay_sig_for_api("12345", "/wxa/game/getbalance", post_body),
      "11bac6388871d29c055c7d16fbe42e8d646855b666faf89b15c815218b1b23bd"
    );
  }

  #[test]
  fn signature_matches_official_vector() {
    // 官方示例向量：session_key 原串作 key、post_body 固定串 → 42fe1d33...。
    let post_body =
      r#"{"offer_id": "12345678", "openid": "oUrsfxxxxxxxxxx", "ts": 1668136271, "zone_id": "1", "env": 0}"#;
    assert_eq!(
      signature_with_session_key("9hAb/NEYUlkaMBEsmFgzig==", post_body),
      "42fe1d3341fb1c8bd6f5014ba735ab04eacc80a2deb3ab4669eab4700b5b6729"
    );
  }

  #[test]
  fn sign_data_is_canonical_json_with_sorted_keys() {
    let sign_data = build_sign_data(&SignDataParams {
      offer_id: "offer-1",
      env: 0,
      product_id: "goods_month",
      goods_price_cents: 800,
      out_trade_no: "pg0001abc",
      buy_quantity: 1,
      attach: "membership",
    });
    assert_eq!(
      sign_data,
      "{\"attach\":\"membership\",\"buyQuantity\":1,\"currencyType\":\"CNY\",\"env\":0,\
       \"goodsPrice\":800,\"offerId\":\"offer-1\",\"outTradeNo\":\"pg0001abc\",\"productId\":\"goods_month\"}"
    );
    // 键名字典序断言：attach < buyQuantity < currencyType < env < goodsPrice < offerId < outTradeNo < productId。
    let keys = json_keys(&sign_data);
    assert_eq!(
      keys,
      vec!["attach", "buyQuantity", "currencyType", "env", "goodsPrice", "offerId", "outTradeNo", "productId"]
    );
  }

  #[test]
  fn sign_data_escapes_attach() {
    let sign_data = build_sign_data(&SignDataParams {
      offer_id: "o",
      env: 1,
      product_id: "p",
      goods_price_cents: 1,
      out_trade_no: "n",
      buy_quantity: 1,
      attach: "say\"hi\"\\",
    });
    assert!(sign_data.contains("say\\\"hi\\\"\\\\"), "attach 转义: {sign_data}");
  }

  #[test]
  fn queried_order_state_gates() {
    let paid = QueriedOrder { status: Some(2), ..QueriedOrder::for_test() };
    assert!(paid.is_paid());
    let delivered = QueriedOrder { status: Some(4), ..QueriedOrder::for_test() };
    assert!(delivered.is_paid());
    let init = QueriedOrder { status: Some(1), ..QueriedOrder::for_test() };
    assert!(!init.is_paid());
    let ios_refund = QueriedOrder { status: Some(2), order_type: Some(8), ..QueriedOrder::for_test() };
    assert!(ios_refund.is_refunded(), "Apple IAP 退款单（order_type=8）");
    let refunded = QueriedOrder { status: Some(8), ..QueriedOrder::for_test() };
    assert!(refunded.is_refunded());
    let closed = QueriedOrder { status: Some(6), ..QueriedOrder::for_test() };
    assert!(closed.is_closed());
  }

  impl QueriedOrder {
    fn for_test() -> Self {
      Self {
        order_id: None,
        status: None,
        order_fee: None,
        paid_fee: None,
        refund_fee: None,
        order_type: None,
        paid_time: None,
        provide_time: None,
        wx_order_id: None,
        env_type: None,
      }
    }
  }
}
