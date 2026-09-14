//! 微信「消息推送」通道原语（p053 D1 平台推送面；业务无关、可复用）：
//! Token-sha1 验签（明文模式）、WXBizMsgCrypt AES-256-CBC 解密（安全模式）、
//! XML / JSON 二态事件解析、应答体构造（与请求同格式，`ErrCode=0` 成功）。
//!
//! 官方口径锚点：
//! - 验签：明文模式 `sha1(字典序(Token, timestamp, nonce))` 对 URL `signature`；
//!   安全模式 `sha1(字典序(Token, timestamp, nonce, Encrypt))` 对 `msg_signature`；
//! - 密文：AESKey = Base64Decode(EncodingAESKey + "=")（32 字节），IV = AESKey
//!   前 16 字节，AES-256-CBC + PKCS#7；明文帧 = random(16) + msg_len(4, 网络
//!   字节序) + msg + appid——解密后须校验 appid 与自身相符；
//! - 应答：与请求同格式（JSON 对 JSON / XML 对 XML），xpay 特定应答体
//!   `<xml><ErrCode>0</ErrCode><ErrMsg>success</ErrMsg></xml>` /
//!   `{"ErrCode":0,"ErrMsg":"success"}`，失败平台重推 ≤15 次。

use aes::Aes256;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use cbc::Decryptor;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use sha1::Sha1;

type Aes256CbcDecryptor = Decryptor<Aes256>;

#[derive(Debug, thiserror::Error)]
pub enum PushError {
  /// 验签失败 / 报文畸形（请求侧——拒绝处理，应答失败让平台重试或丢弃）。
  #[error("push rejected: {0}")]
  Invalid(String),
  /// 通道配置缺失 / 解密依赖不可用（运营配置面——应答失败留痕）。
  #[error("push unavailable: {0}")]
  Unavailable(String),
}

/// 推送数据格式（MP 消息推送配置项「数据格式」二态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushFormat {
  Xml,
  Json,
}

/// Token-sha1 验签（明文模式）：字典序拼接后 sha1 十六进制比对（常量时间）。
pub fn verify_sha1_signature(token: &str, timestamp: &str, nonce: &str, signature: &str) -> bool {
  let mut parts = [token, timestamp, nonce];
  parts.sort_unstable();
  constant_time_eq(sha1_digest(parts.concat().as_bytes()).as_bytes(), signature.as_bytes())
}

fn sha1_digest(bytes: &[u8]) -> String {
  use sha1::Digest;
  hex::encode(Sha1::digest(bytes))
}

/// 安全模式 msg_signature 验签：sha1(字典序(Token, timestamp, nonce, Encrypt))。
pub fn verify_sha1_msg_signature(
  token: &str,
  timestamp: &str,
  nonce: &str,
  encrypt: &str,
  msg_signature: &str,
) -> bool {
  let mut parts = [token, timestamp, nonce, encrypt];
  parts.sort_unstable();
  constant_time_eq(sha1_digest(parts.concat().as_bytes()).as_bytes(), msg_signature.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
  if a.len() != b.len() {
    return false;
  }
  a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 密文解密（WXBizMsgCrypt 口径）+ appid 校验，返回明文消息体。
pub fn decrypt_aes_message(
  encoding_aes_key: &str,
  ciphertext_b64: &str,
  expected_appid: &str,
) -> Result<String, PushError> {
  let key = aes_key(encoding_aes_key)?;
  let ciphertext = BASE64_STANDARD
    .decode(ciphertext_b64)
    .map_err(|e| PushError::Invalid(format!("ciphertext base64: {e}")))?;
  let (iv, _) = key.split_at(16);
  let decryptor =
    Aes256CbcDecryptor::new_from_slices(&key, iv).map_err(|e| PushError::Unavailable(format!("aes init: {e}")))?;
  let plaintext = decryptor
    .decrypt_padded_vec_mut::<Pkcs7>(&ciphertext)
    .map_err(|e| PushError::Invalid(format!("aes decrypt: {e}")))?;
  // 帧结构：random(16) + msg_len(4, 网络字节序) + msg + appid。
  if plaintext.len() < 20 {
    return Err(PushError::Invalid("plaintext frame too short".to_string()));
  }
  let msg_len = u32::from_be_bytes([plaintext[16], plaintext[17], plaintext[18], plaintext[19]]) as usize;
  let body = &plaintext[20..];
  if body.len() < msg_len {
    return Err(PushError::Invalid("plaintext msg_len out of range".to_string()));
  }
  let msg = &body[..msg_len];
  let appid = String::from_utf8_lossy(&body[msg_len..]).to_string();
  if !expected_appid.is_empty() && appid != expected_appid {
    return Err(PushError::Invalid(format!("appid mismatch: {appid}")));
  }
  String::from_utf8(msg.to_vec()).map_err(|e| PushError::Invalid(format!("msg utf-8: {e}")))
}

/// AESKey = Base64Decode(EncodingAESKey + "=")（必须 32 字节）。
fn aes_key(encoding_aes_key: &str) -> Result<[u8; 32], PushError> {
  let with_pad = format!("{encoding_aes_key}=");
  let key = BASE64_STANDARD
    .decode(with_pad.as_bytes())
    .map_err(|e| PushError::Unavailable(format!("encoding aes key base64: {e}")))?;
  key.try_into().map_err(|_| PushError::Unavailable("aes key must be 32 bytes".to_string()))
}

// ---------------------------------------------------------------------------
// 事件解析（XML / JSON 二态 → 结构化事件；未知事件保留原始字段面）
// ---------------------------------------------------------------------------

/// xpay 推送事件（p053 D8 四类中适用三类：发货 / 退款 / 投诉；代币支付不适用
/// 道具直购——收到按未知事件留痕）。
#[derive(Debug, Clone)]
pub enum XpayEvent {
  /// 发货推送（Event = xpay_goods_deliver_notify）。
  GoodsDeliverNotify { openid: String, out_trade_no: String, wx_order_id: String, product_id: String, quantity: i64 },
  /// 退款推送（Event = xpay_refund_notify；Apple IAP 退款成功亦推）。
  RefundNotify { openid: String, out_trade_no: String, wx_order_id: String },
  /// 其他事件（投诉等 v1 人工面——原始字段留痕供 MP 后台处理）。
  Other { event: String, fields: Vec<(String, String)> },
}

/// 按请求 Content-Type / 体形态嗅探格式。
pub fn sniff_format(content_type: Option<&str>, body: &str) -> PushFormat {
  if let Some(ct) = content_type {
    if ct.contains("json") {
      return PushFormat::Json;
    }
    if ct.contains("xml") {
      return PushFormat::Xml;
    }
  }
  if body.trim_start().starts_with('{') { PushFormat::Json } else { PushFormat::Xml }
}

/// 解析事件（已解密 / 明文的 JSON 或 XML 体）。
pub fn parse_event(format: PushFormat, body: &str) -> Result<XpayEvent, PushError> {
  match format {
    PushFormat::Json => parse_event_json(body),
    PushFormat::Xml => parse_event_xml(body),
  }
}

fn parse_event_json(body: &str) -> Result<XpayEvent, PushError> {
  let value: serde_json::Value =
    serde_json::from_str(body).map_err(|e| PushError::Invalid(format!("json parse: {e}")))?;
  let obj = value.as_object().ok_or_else(|| PushError::Invalid("json body not object".to_string()))?;
  let get = |key: &str| obj.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
  // openid 双源：xpay 业务字段 OpenId，回落消息信封 FromUserName（通用口径）。
  let openid = {
    let id = get("OpenId");
    if id.is_empty() { get("FromUserName") } else { id }
  };
  let event = get("Event");
  match event.as_str() {
    "xpay_goods_deliver_notify" => Ok(XpayEvent::GoodsDeliverNotify {
      openid,
      out_trade_no: get("OutTradeNo"),
      wx_order_id: obj
        .get("WeChatPayInfo")
        .and_then(|info| info.get("MchOrderNo"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string(),
      product_id: obj
        .get("GoodsInfo")
        .and_then(|info| info.get("ProductId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string(),
      quantity: obj.get("GoodsInfo").and_then(|info| info.get("Quantity")).and_then(|v| v.as_i64()).unwrap_or(1),
    }),
    "xpay_refund_notify" => Ok(XpayEvent::RefundNotify {
      openid,
      out_trade_no: get("OutTradeNo"),
      wx_order_id: obj
        .get("WeChatPayInfo")
        .and_then(|info| info.get("MchOrderNo"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string(),
    }),
    _ => Ok(XpayEvent::Other { event, fields: flatten_json(obj) }),
  }
}

fn flatten_json(obj: &serde_json::Map<String, serde_json::Value>) -> Vec<(String, String)> {
  let mut out = Vec::new();
  for (key, value) in obj {
    match value {
      serde_json::Value::String(s) => out.push((key.clone(), s.clone())),
      serde_json::Value::Number(n) => out.push((key.clone(), n.to_string())),
      serde_json::Value::Bool(b) => out.push((key.clone(), b.to_string())),
      serde_json::Value::Object(nested) => {
        for (k, v) in flatten_json(nested) {
          out.push((format!("{key}.{k}"), v));
        }
      }
      _ => {}
    }
  }
  out
}

/// 极简 XML 取值（微信推送报文为扁平 / 二层结构，无属性面需求；CDATA 剥除）。
fn xml_tag<'a>(body: &'a str, path: &[&str]) -> Option<&'a str> {
  if path.is_empty() {
    return None;
  }
  // 逐层定位开标签区间。
  let mut scope = body;
  for tag in path {
    let open = format!("<{tag}>");
    let open_cdata = format!("<{tag}><![CDATA[");
    let start = scope
      .find(&open_cdata)
      .map(|i| i + open_cdata.len())
      .or_else(|| scope.find(&open).map(|i| i + open.len()))?;
    let tail = &scope[start..];
    let end = tail.find(&format!("</{tag}>"))?;
    scope = &tail[..end];
  }
  Some(scope.trim_end_matches("]]>"))
}

/// openid 双源（OpenId 业务字段优先，回落信封 FromUserName）。
fn xml_openid(body: &str) -> String {
  xml_tag(body, &["OpenId"])
    .or_else(|| xml_tag(body, &["FromUserName"]))
    .unwrap_or_default()
    .to_string()
}

/// XML 顶层字段平铺（`Other` 事件留痕面）：扫全部 `<tag>文本</tag>` 对，
/// CDATA 剥除、空文本跳过；二层结构（如 `WeChatPayInfo.MchOrderNo`）的内层
/// 键会被独立采到（留痕容忍扁平化，与 JSON 面 `flatten_json` 同口径）。
fn xml_top_level_fields(body: &str) -> Vec<(String, String)> {
  let mut out = Vec::new();
  let mut rest = body;
  while let Some(open) = rest.find('<') {
    let after = &rest[open + 1..];
    let Some(tag_end) = after.find('>') else { break };
    let tag = &after[..tag_end];
    if tag.starts_with('/') || tag.starts_with('?') || tag.starts_with('!') {
      rest = &after[tag_end + 1..];
      continue;
    }
    let tail = &after[tag_end + 1..];
    // CDATA 文本段 `<tag><![CDATA[value]]></tag>`：内容以 `<` 开头，纯文本段
    // 逻辑会误判为空——须按 `]]>` 收口取值。
    if let Some(content) = tail.strip_prefix("<![CDATA[") {
      let end = content.find("]]>").unwrap_or(content.len());
      let value = content[..end].trim();
      if !value.is_empty() {
        out.push((tag.to_string(), value.to_string()));
      }
      rest = &content[end..];
      continue;
    }
    let text_end = tail.find('<').unwrap_or(tail.len());
    let value = tail[..text_end].trim();
    if !value.is_empty() {
      out.push((tag.to_string(), value.to_string()));
    }
    rest = &tail[text_end..];
  }
  out
}

fn parse_event_xml(body: &str) -> Result<XpayEvent, PushError> {
  let event = xml_tag(body, &["Event"]).unwrap_or_default().to_string();
  match event.as_str() {
    "xpay_goods_deliver_notify" => Ok(XpayEvent::GoodsDeliverNotify {
      openid: xml_openid(body),
      out_trade_no: xml_tag(body, &["OutTradeNo"]).unwrap_or_default().to_string(),
      wx_order_id: xml_tag(body, &["WeChatPayInfo", "MchOrderNo"]).unwrap_or_default().to_string(),
      product_id: xml_tag(body, &["GoodsInfo", "ProductId"]).unwrap_or_default().to_string(),
      quantity: xml_tag(body, &["GoodsInfo", "Quantity"]).unwrap_or("1").parse().unwrap_or(1),
    }),
    "xpay_refund_notify" => Ok(XpayEvent::RefundNotify {
      openid: xml_openid(body),
      out_trade_no: xml_tag(body, &["OutTradeNo"]).unwrap_or_default().to_string(),
      wx_order_id: xml_tag(body, &["WeChatPayInfo", "MchOrderNo"]).unwrap_or_default().to_string(),
    }),
    _ => Ok(XpayEvent::Other { event, fields: xml_top_level_fields(body) }),
  }
}

/// 成功应答体（与请求同格式；空 / success 等价成功是通用口径，xpay 特定
/// 应答 = ErrCode 0 面）。
pub fn success_reply(format: PushFormat) -> String {
  match format {
    PushFormat::Xml => "<xml><ErrCode>0</ErrCode><ErrMsg>success</ErrMsg></xml>".to_string(),
    PushFormat::Json => r#"{"ErrCode":0,"ErrMsg":"success"}"#.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sha1_signature_verifies_sorted_concat() {
    // 官方消息推送示例：Token=AAAAA、timestamp=1714036504、nonce=1514711492
    // → signature=f464b24fc39322e44b38aa78f5edd27bd1441696（字典序拼接 sha1）。
    assert!(verify_sha1_signature("AAAAA", "1714036504", "1514711492", "f464b24fc39322e44b38aa78f5edd27bd1441696"));
    assert!(!verify_sha1_signature("AAAAA", "1714036504", "1514711492", "deadbeef"));
  }

  #[test]
  fn aes_roundtrip_decrypts_frame() {
    // 构造明文帧：random16 + len(4, BE) + msg + appid，AES-256-CBC + PKCS7 加密
    // 后走 decrypt_aes_message 回环（iv = key 前 16 字节，WXBizMsgCrypt 口径）。
    use cbc::cipher::BlockEncryptMut;

    let msg =
      r#"{"ToUserName":"gh_x","FromUserName":"openid-1","MsgType":"event","Event":"xpay_goods_deliver_notify"}"#;
    let appid = "wxappid123";
    let key = [7u8; 32];
    let aes_key_b64 = BASE64_STANDARD.encode(key);
    let encoding_aes_key = aes_key_b64.trim_end_matches('=');
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0u8; 16]);
    frame.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    frame.extend_from_slice(msg.as_bytes());
    frame.extend_from_slice(appid.as_bytes());
    let iv = &key[..16];
    let encryptor = cbc::Encryptor::<Aes256>::new_from_slices(&key, iv).unwrap();
    let padded = encryptor.encrypt_padded_vec_mut::<Pkcs7>(&frame);
    let ciphertext = BASE64_STANDARD.encode(padded);
    let decrypted = decrypt_aes_message(encoding_aes_key, &ciphertext, appid).expect("decrypt");
    assert_eq!(decrypted, msg);
    // appid 不符拒收。
    assert!(decrypt_aes_message(encoding_aes_key, &ciphertext, "other-app").is_err());
  }

  #[test]
  fn parses_xml_deliver_notify() {
    let body = "<xml><ToUserName><![CDATA[gh_x]]></ToUserName><FromUserName><![CDATA[oOpenId]]></FromUserName>\
                <Event><![CDATA[xpay_goods_deliver_notify]]></Event><OutTradeNo><![CDATA[pg0001abc]]></OutTradeNo>\
                <WeChatPayInfo><MchOrderNo><![CDATA[4200001]]></MchOrderNo></WeChatPayInfo>\
                <GoodsInfo><ProductId><![CDATA[goods_month]]></ProductId><Quantity>1</Quantity></GoodsInfo></xml>";
    match parse_event(PushFormat::Xml, body).expect("parse") {
      XpayEvent::GoodsDeliverNotify { openid, out_trade_no, wx_order_id, product_id, quantity } => {
        assert_eq!(openid, "oOpenId");
        assert_eq!(out_trade_no, "pg0001abc");
        assert_eq!(wx_order_id, "4200001");
        assert_eq!(product_id, "goods_month");
        assert_eq!(quantity, 1);
      }
      other => panic!("unexpected event: {other:?}"),
    }
  }

  #[test]
  fn parses_json_refund_notify() {
    let body = r#"{"ToUserName":"gh_x","FromUserName":"oOpenId","MsgType":"event","Event":"xpay_refund_notify","OutTradeNo":"pg0001abc","WeChatPayInfo":{"MchOrderNo":"4200001"}}"#;
    match parse_event(PushFormat::Json, body).expect("parse") {
      XpayEvent::RefundNotify { openid, out_trade_no, wx_order_id } => {
        assert_eq!(openid, "oOpenId");
        assert_eq!(out_trade_no, "pg0001abc");
        assert_eq!(wx_order_id, "4200001");
      }
      other => panic!("unexpected event: {other:?}"),
    }
  }

  #[test]
  fn unknown_event_kept_raw() {
    let body = r#"{"Event":"xpay_complaint_notify","ComplaintId":"c-1"}"#;
    match parse_event(PushFormat::Json, body).expect("parse") {
      XpayEvent::Other { event, fields } => {
        assert_eq!(event, "xpay_complaint_notify");
        assert!(fields.iter().any(|(k, v)| k == "ComplaintId" && v == "c-1"));
      }
      other => panic!("unexpected event: {other:?}"),
    }
  }

  #[test]
  fn unknown_xml_event_keeps_fields_for_manual_processing() {
    let body = "<xml><ToUserName><![CDATA[gh_x]]></ToUserName>\
                <Event><![CDATA[xpay_complaint_notify]]></Event>\
                <ComplaintId><![CDATA[c-9]]></ComplaintId><OrderStatus>3</OrderStatus></xml>";
    match parse_event(PushFormat::Xml, body).expect("parse") {
      XpayEvent::Other { event, fields } => {
        assert_eq!(event, "xpay_complaint_notify");
        assert!(fields.iter().any(|(k, v)| k == "ComplaintId" && v == "c-9"), "CDATA 剥除");
        assert!(fields.iter().any(|(k, v)| k == "OrderStatus" && v == "3"), "纯文本字段");
        assert!(fields.iter().all(|(k, _)| !k.is_empty()));
      }
      other => panic!("unexpected event: {other:?}"),
    }
  }

  #[test]
  fn sniff_and_reply_follow_request_format() {
    assert_eq!(sniff_format(Some("application/json; charset=utf-8"), ""), PushFormat::Json);
    assert_eq!(sniff_format(Some("text/xml"), ""), PushFormat::Xml);
    assert_eq!(sniff_format(None, "  {\"a\":1}"), PushFormat::Json);
    assert_eq!(sniff_format(None, "<xml></xml>"), PushFormat::Xml);
    assert_eq!(success_reply(PushFormat::Xml), "<xml><ErrCode>0</ErrCode><ErrMsg>success</ErrMsg></xml>");
    assert_eq!(success_reply(PushFormat::Json), r#"{"ErrCode":0,"ErrMsg":"success"}"#);
  }
}
