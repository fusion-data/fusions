//! ACS3-HMAC-SHA256 签名实现 —— `RequestSigner` impl `reqsign_core::SignRequest`。

use std::time::Duration;

use chrono::{DateTime, Utc};
use http::HeaderValue;
use http::header::{AUTHORIZATION, HOST};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqsign_core::hash::{hex_hmac_sha256, hex_sha256};
use reqsign_core::{Context, Error, Result, SignRequest};

use super::credential::Credential;

/// ACS3 算法标识。
const ACS3_ALGORITHM: &str = "ACS3-HMAC-SHA256";

/// `x-acs-content-sha256` 的 header name。
const X_ACS_CONTENT_SHA256: &str = "x-acs-content-sha256";
/// `x-acs-date` 的 header name。
const X_ACS_DATE: &str = "x-acs-date";
/// `x-acs-signature-nonce` 的 header name。
const X_ACS_SIGNATURE_NONCE: &str = "x-acs-signature-nonce";
/// `x-acs-security-token` 的 header name（STS 临时凭据时填）。
const X_ACS_SECURITY_TOKEN: &str = "x-acs-security-token";

/// 阿里云 v3 path / query percent-encoding 字符集（与 RFC3986 unreserved 一致：保留 A-Z a-z 0-9 - _ . ~）。
const ACS3_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// 阿里云 ACS3 请求签名器。
///
/// 调用约定：
/// - caller 构造 `http::Request<Bytes>` 时**必须**带上 `x-acs-action` / `x-acs-version` 等业务 header（这些是阿里云每个产品 / 接口决定的，签名器不感知）。
/// - signer 自动补：`host`（从 URI 推）、`x-acs-date`、`x-acs-signature-nonce`、`x-acs-content-sha256`、`x-acs-security-token`（如有）、`Authorization`。
/// - 业务调用时把 body 也喂给 signer（payload hash 必须基于真实 body），caller 通常先 `into_parts()` 拆出 body 提供 hash，再用 Signer 签 parts。
///
/// 测试场景可经 `with_signing_time` / `with_signature_nonce` 注入确定值，得到可复现签名。
#[derive(Debug, Default, Clone)]
pub struct RequestSigner {
  /// 测试用：固定签名时间；生产留 `None` 走 `Utc::now()`。
  signing_time: Option<DateTime<Utc>>,
  /// 测试用：固定 nonce；生产留 `None` 走 uuid v7。
  signature_nonce: Option<String>,
  /// 当 caller 已提供 payload hash（比如 streaming body 不便重读），可经 `with_payload_hash` 注入；
  /// 否则签名器看 `x-acs-content-sha256` header；都没有时按空 payload（sha256("")) 走。
  payload_hash: Option<String>,
}

impl RequestSigner {
  pub fn new() -> Self {
    Self::default()
  }

  /// 测试钩：固定签名时间。**仅供单元测试**对照阿里云 reference vector，
  /// 生产代码勿调（错位 signing_time 会被阿里云以 `signature_does_not_match`
  /// 拒绝，但消费者只看到 5xx 不知根因）。`#[doc(hidden)]` 让 IDE autocomplete
  /// 不显示。
  #[doc(hidden)]
  pub fn with_signing_time(mut self, time: DateTime<Utc>) -> Self {
    self.signing_time = Some(time);
    self
  }

  /// 测试钩：固定签名 nonce。**仅供单元测试**确定性签名复现，生产代码勿调
  /// （nonce 重用会触发阿里云重放检测）。
  #[doc(hidden)]
  pub fn with_signature_nonce(mut self, nonce: impl Into<String>) -> Self {
    self.signature_nonce = Some(nonce.into());
    self
  }

  pub fn with_payload_hash(mut self, hash: impl Into<String>) -> Self {
    self.payload_hash = Some(hash.into());
    self
  }

  fn now(&self) -> DateTime<Utc> {
    self.signing_time.unwrap_or_else(Utc::now)
  }

  /// 默认用 uuid v4（122 bit 全随机）。阿里云 ACS3 只要求 `x-acs-signature-nonce`
  /// 唯一（防重放），v4 满足该需求；签名器是安全敏感面，默认取最保守值 ——
  /// 不用 v7（前 48 bit 是可推测的 ms 时间戳前缀）。caller 若需确定性复现可用
  /// [`Self::with_signature_nonce`] 覆盖为固定值。
  fn nonce(&self) -> String {
    self.signature_nonce.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
  }
}

impl SignRequest for RequestSigner {
  type Credential = Credential;

  async fn sign_request(
    &self,
    _ctx: &Context,
    req: &mut http::request::Parts,
    credential: Option<&Self::Credential>,
    _expires_in: Option<Duration>,
  ) -> Result<()> {
    let Some(cred) = credential else {
      return Err(Error::credential_invalid("aliyun-acs3 requires credential"));
    };

    // ---- payload hash ----
    let payload_hash = if let Some(h) = &self.payload_hash {
      h.clone()
    } else if let Some(v) = req.headers.get(X_ACS_CONTENT_SHA256) {
      v.to_str()
        .map_err(|e| Error::request_invalid(format!("x-acs-content-sha256 not ASCII: {e}")))?
        .to_string()
    } else {
      // 空 payload 的 sha256
      hex_sha256(b"")
    };

    // ---- 时间 / nonce ----
    let date = self.now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let nonce = self.nonce();

    // ---- 自动补 host / x-acs-date / x-acs-signature-nonce / x-acs-content-sha256 / x-acs-security-token ----
    if !req.headers.contains_key(HOST)
      && let Some(authority) = req.uri.authority()
    {
      req.headers.insert(
        HOST,
        HeaderValue::from_str(authority.host())
          .map_err(|e| Error::request_invalid(format!("invalid host header from URI: {e}")))?,
      );
    }
    insert_if_missing(&mut req.headers, X_ACS_DATE, &date)?;
    insert_if_missing(&mut req.headers, X_ACS_SIGNATURE_NONCE, &nonce)?;
    // 当 caller 通过 `with_payload_hash` 显式注入 hash 时，强制覆盖
    // `x-acs-content-sha256` header 保证签名基础与发送 header 一致。否则
    // caller 同时设两者且不同值时，签名用 with_payload_hash 而 header 用旧值
    // → 阿里云算签名失败 401，但 caller 拿到 5xx 不知根因（A-L1）。
    if self.payload_hash.is_some() {
      let v = HeaderValue::from_str(&payload_hash)
        .map_err(|e| Error::request_invalid(format!("invalid x-acs-content-sha256: {e}")))?;
      req.headers.insert(X_ACS_CONTENT_SHA256, v);
    } else {
      insert_if_missing(&mut req.headers, X_ACS_CONTENT_SHA256, &payload_hash)?;
    }
    if let Some(tok) = &cred.security_token {
      insert_if_missing(&mut req.headers, X_ACS_SECURITY_TOKEN, tok)?;
    }

    // ---- canonical 形式 ----
    let method = req.method.as_str();
    let canonical_uri = canonicalize_uri(req.uri.path());
    let canonical_query = canonicalize_query(req.uri.query().unwrap_or(""));

    // signed headers = headers 全集（除 Authorization 自身）按 lower-case 字典序
    let mut header_pairs: Vec<(String, String)> = req
      .headers
      .iter()
      .filter(|(name, _)| !name.as_str().eq_ignore_ascii_case("authorization"))
      .map(|(name, value)| {
        let v = value
          .to_str()
          .map_err(|e| Error::request_invalid(format!("header {name} not ASCII: {e}")))?
          .trim()
          .to_string();
        Ok::<_, Error>((name.as_str().to_ascii_lowercase(), v))
      })
      .collect::<Result<Vec<_>>>()?;
    header_pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers = header_pairs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect::<String>();
    let signed_headers = header_pairs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let canonical_request =
      format!("{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    // ---- string to sign + signature ----
    let string_to_sign = format!("{ACS3_ALGORITHM}\n{}", hex_sha256(canonical_request.as_bytes()));
    let signature = hex_hmac_sha256(cred.access_key_secret.as_bytes(), string_to_sign.as_bytes());

    // ---- Authorization header ----
    let authorization = format!(
      "{ACS3_ALGORITHM} Credential={ak},SignedHeaders={signed},Signature={sig}",
      ak = cred.access_key_id,
      signed = signed_headers,
      sig = signature,
    );
    req.headers.insert(
      AUTHORIZATION,
      HeaderValue::from_str(&authorization)
        .map_err(|e| Error::request_invalid(format!("Authorization header invalid: {e}")))?,
    );

    Ok(())
  }
}

/// 按 RFC3986 对 path 做 percent-encode，保留 `/` 分隔。空 path 视作 `/`。
///
/// 输入约定：`path` 来自 `http::Uri::path()`，**已是 percent-encoded 形式**。
/// 因此对每个 segment 只编码尚未编码的字节，遇到已有的 `%XX` 转义三元组
/// 原样透传 —— 否则会把 `%E4` 再编码成 `%25E4`（双重编码），导致阿里云
/// ACS3 canonical request 与服务端算的不一致而 `signature_does_not_match`。
fn canonicalize_uri(path: &str) -> String {
  let path = if path.is_empty() { "/" } else { path };
  path.split('/').map(encode_path_segment).collect::<Vec<_>>().join("/")
}

/// 对单个 path segment 做 ACS3 percent-encoding，保留输入中已有的 `%XX` 转义。
///
/// ACS3 unreserved 集合 = 字母数字 + `-` `_` `.` `~`（与 [`ACS3_ENCODE_SET`] 一致）。
fn encode_path_segment(seg: &str) -> String {
  let bytes = seg.as_bytes();
  let mut out = String::with_capacity(seg.len());
  let mut i = 0;
  while i < bytes.len() {
    let b = bytes[i];
    if b == b'%' && i + 2 < bytes.len() && bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit() {
      // 已是合法 percent 转义 —— 原样透传，不再编码。
      out.push_str(&seg[i..i + 3]);
      i += 3;
    } else if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
      out.push(b as char);
      i += 1;
    } else {
      out.push('%');
      out.push_str(&format!("{b:02X}"));
      i += 1;
    }
  }
  out
}

/// 把 query string 解析 → 按 key 字典序排列 → key/value 各自 percent-encode → join '&'。
///
/// 阿里云 v3 规则：value 为空时 form 为 `key=`（不省略 `=`）。
fn canonicalize_query(query: &str) -> String {
  if query.is_empty() {
    return String::new();
  }
  let mut pairs: Vec<(String, String)> = query
    .split('&')
    .filter(|p| !p.is_empty())
    .map(|pair| {
      let mut iter = pair.splitn(2, '=');
      let k = iter.next().unwrap_or("").to_string();
      let v = iter.next().unwrap_or("").to_string();
      (k, v)
    })
    .collect();
  pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
  pairs
    .into_iter()
    .map(|(k, v)| format!("{}={}", utf8_percent_encode(&k, ACS3_ENCODE_SET), utf8_percent_encode(&v, ACS3_ENCODE_SET)))
    .collect::<Vec<_>>()
    .join("&")
}

fn insert_if_missing(headers: &mut http::HeaderMap, name: &'static str, value: &str) -> Result<()> {
  if headers.contains_key(name) {
    return Ok(());
  }
  let v =
    HeaderValue::from_str(value).map_err(|e| Error::request_invalid(format!("header {name} value invalid: {e}")))?;
  headers.insert(http::HeaderName::from_static(name), v);
  Ok(())
}

/// 纯函数版本 —— 给上层做 hard-coded vector 测试 / 对照阿里云 docs reference 用。
///
/// 输入要求：`canonical_headers` 已按 lower-case 排好序、`signed_headers` 与之同序的 ';' join。
/// 返回 `(string_to_sign, signature, authorization_header)`。
pub fn sign_acs3(
  access_key_id: &str,
  access_key_secret: &str,
  method: &str,
  canonical_uri: &str,
  canonical_query: &str,
  canonical_headers: &str,
  signed_headers: &str,
  payload_hash: &str,
) -> (String, String, String) {
  let canonical_request =
    format!("{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
  let string_to_sign = format!("{ACS3_ALGORITHM}\n{}", hex_sha256(canonical_request.as_bytes()));
  let signature = hex_hmac_sha256(access_key_secret.as_bytes(), string_to_sign.as_bytes());
  let authorization =
    format!("{ACS3_ALGORITHM} Credential={access_key_id},SignedHeaders={signed_headers},Signature={signature}");
  (string_to_sign, signature, authorization)
}

#[cfg(test)]
mod tests {
  use super::*;
  use bytes::Bytes;
  use chrono::TimeZone;
  use http::Request;
  use reqsign_core::Signer;

  use super::super::credential::StaticCredentialProvider;

  fn fixed_signer() -> RequestSigner {
    RequestSigner::new()
      .with_signing_time(Utc.with_ymd_and_hms(2026, 5, 8, 12, 0, 0).unwrap())
      .with_signature_nonce("fixed-nonce-123")
  }

  fn build_send_sms_request() -> Request<Bytes> {
    Request::builder()
      .method("POST")
      .uri("https://dysmsapi.aliyuncs.com/?PhoneNumbers=13800138000&SignName=Hylx&TemplateCode=SMS_001")
      .header("x-acs-action", "SendSms")
      .header("x-acs-version", "2017-05-25")
      .body(Bytes::new())
      .unwrap()
  }

  #[tokio::test]
  async fn signature_is_deterministic_for_fixed_inputs() {
    let cred = Credential::new("LTAItestkey", "testsecret");
    let signer1 = fixed_signer();
    let signer2 = fixed_signer();

    let mut parts1 = build_send_sms_request().into_parts().0;
    let mut parts2 = build_send_sms_request().into_parts().0;
    let ctx = Context::new();
    signer1.sign_request(&ctx, &mut parts1, Some(&cred), None).await.unwrap();
    signer2.sign_request(&ctx, &mut parts2, Some(&cred), None).await.unwrap();

    let auth1 = parts1.headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
    let auth2 = parts2.headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
    assert_eq!(auth1, auth2, "同输入应产生同签名");
  }

  #[tokio::test]
  async fn signature_changes_with_secret() {
    let signer = fixed_signer();
    let ctx = Context::new();

    let mut parts_a = build_send_sms_request().into_parts().0;
    let mut parts_b = build_send_sms_request().into_parts().0;
    signer
      .sign_request(&ctx, &mut parts_a, Some(&Credential::new("ak", "secret_a")), None)
      .await
      .unwrap();
    signer
      .sign_request(&ctx, &mut parts_b, Some(&Credential::new("ak", "secret_b")), None)
      .await
      .unwrap();

    let auth_a = parts_a.headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
    let auth_b = parts_b.headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
    assert_ne!(auth_a, auth_b);
  }

  #[tokio::test]
  async fn missing_credential_returns_error() {
    let signer = fixed_signer();
    let ctx = Context::new();
    let mut parts = build_send_sms_request().into_parts().0;
    let result = signer.sign_request(&ctx, &mut parts, None, None).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn host_header_auto_inserted_from_uri() {
    let cred = Credential::new("ak", "sk");
    let signer = fixed_signer();
    let ctx = Context::new();
    let mut parts = build_send_sms_request().into_parts().0;
    signer.sign_request(&ctx, &mut parts, Some(&cred), None).await.unwrap();
    assert_eq!(parts.headers.get(HOST).unwrap().to_str().unwrap(), "dysmsapi.aliyuncs.com");
  }

  #[tokio::test]
  async fn x_acs_required_headers_set() {
    let cred = Credential::new("ak", "sk");
    let signer = fixed_signer();
    let ctx = Context::new();
    let mut parts = build_send_sms_request().into_parts().0;
    signer.sign_request(&ctx, &mut parts, Some(&cred), None).await.unwrap();
    assert_eq!(parts.headers.get(X_ACS_DATE).unwrap().to_str().unwrap(), "2026-05-08T12:00:00Z");
    assert_eq!(parts.headers.get(X_ACS_SIGNATURE_NONCE).unwrap().to_str().unwrap(), "fixed-nonce-123");
    // 空 payload sha256 = e3b0c44...
    assert_eq!(
      parts.headers.get(X_ACS_CONTENT_SHA256).unwrap().to_str().unwrap(),
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
  }

  #[tokio::test]
  async fn security_token_header_added_when_present() {
    let cred = Credential::new("ak", "sk").with_security_token("stsTok");
    let signer = fixed_signer();
    let ctx = Context::new();
    let mut parts = build_send_sms_request().into_parts().0;
    signer.sign_request(&ctx, &mut parts, Some(&cred), None).await.unwrap();
    assert_eq!(parts.headers.get(X_ACS_SECURITY_TOKEN).unwrap().to_str().unwrap(), "stsTok");
  }

  #[tokio::test]
  async fn authorization_header_format_matches_spec() {
    let cred = Credential::new("LTAITestKey", "testSecret");
    let signer = fixed_signer();
    let ctx = Context::new();
    let mut parts = build_send_sms_request().into_parts().0;
    signer.sign_request(&ctx, &mut parts, Some(&cred), None).await.unwrap();
    let auth = parts.headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
    assert!(auth.starts_with("ACS3-HMAC-SHA256 Credential=LTAITestKey,SignedHeaders="));
    assert!(auth.contains(",Signature="));
    // signature 是 64 hex chars
    let sig = auth.rsplit(",Signature=").next().unwrap();
    assert_eq!(sig.len(), 64);
    assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
  }

  #[tokio::test]
  async fn signed_headers_are_lowercase_and_sorted() {
    let cred = Credential::new("ak", "sk");
    let signer = fixed_signer();
    let ctx = Context::new();
    let mut parts = build_send_sms_request().into_parts().0;
    signer.sign_request(&ctx, &mut parts, Some(&cred), None).await.unwrap();
    let auth = parts.headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
    let signed = auth.split(",SignedHeaders=").nth(1).unwrap().split(',').next().unwrap();
    let names: Vec<&str> = signed.split(';').collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "signed headers 必须按字典序");
    for n in &names {
      assert_eq!(*n, n.to_ascii_lowercase(), "signed headers 必须 lowercase");
    }
    // 必须含核心 5 个
    assert!(names.contains(&"host"));
    assert!(names.contains(&"x-acs-action"));
    assert!(names.contains(&"x-acs-version"));
    assert!(names.contains(&"x-acs-date"));
    assert!(names.contains(&"x-acs-content-sha256"));
  }

  #[tokio::test]
  async fn signer_orchestrator_works_end_to_end() {
    let cred = Credential::new("ak", "sk");
    let provider = StaticCredentialProvider::new(cred);
    let request_signer = fixed_signer();
    let signer = Signer::new(Context::new(), provider, request_signer);

    let mut parts = build_send_sms_request().into_parts().0;
    signer.sign(&mut parts, None).await.unwrap();
    assert!(parts.headers.contains_key(AUTHORIZATION));
  }

  // ---- 纯函数 sign_acs3 单测 ----

  #[test]
  fn pure_sign_acs3_deterministic() {
    let (sts1, sig1, auth1) =
      sign_acs3("ak", "sk", "POST", "/", "Action=SendSms", "host:dysmsapi.aliyuncs.com\n", "host", "abc123");
    let (sts2, sig2, auth2) =
      sign_acs3("ak", "sk", "POST", "/", "Action=SendSms", "host:dysmsapi.aliyuncs.com\n", "host", "abc123");
    assert_eq!(sts1, sts2);
    assert_eq!(sig1, sig2);
    assert_eq!(auth1, auth2);
    assert!(sts1.starts_with("ACS3-HMAC-SHA256\n"));
    assert_eq!(sig1.len(), 64);
    assert!(auth1.starts_with("ACS3-HMAC-SHA256 Credential=ak"));
  }

  #[test]
  fn pure_sign_acs3_changes_with_payload() {
    let (_, sig_a, _) = sign_acs3("ak", "sk", "POST", "/", "", "host:x.aliyuncs.com\n", "host", "hash_a");
    let (_, sig_b, _) = sign_acs3("ak", "sk", "POST", "/", "", "host:x.aliyuncs.com\n", "host", "hash_b");
    assert_ne!(sig_a, sig_b);
  }

  #[test]
  fn canonicalize_query_sorts_and_encodes() {
    // alphabetical by key
    assert_eq!(canonicalize_query("b=2&a=1"), "a=1&b=2");
    // empty value preserved as `key=`
    assert_eq!(canonicalize_query("a=&b=2"), "a=&b=2");
    // percent-encode special chars
    assert_eq!(canonicalize_query("k=hello world"), "k=hello%20world");
    assert_eq!(canonicalize_query(""), "");
  }

  #[test]
  fn canonicalize_uri_handles_empty_and_segments() {
    assert_eq!(canonicalize_uri(""), "/");
    assert_eq!(canonicalize_uri("/"), "/");
    assert_eq!(canonicalize_uri("/api/v1/resource"), "/api/v1/resource");
    assert_eq!(canonicalize_uri("/path with space"), "/path%20with%20space");
  }

  #[test]
  fn canonicalize_uri_preserves_existing_percent_escapes() {
    // 输入来自 `Uri::path()`（已编码）—— 已有的 `%XX` 必须原样透传，
    // 不能被再编码成 `%25XX`（双重编码 → 阿里云签名不匹配）。
    assert_eq!(canonicalize_uri("/api/%E4%B8%AD"), "/api/%E4%B8%AD");
    assert_eq!(canonicalize_uri("/a%20b"), "/a%20b");
    // 非转义的裸 `%`（后随非 hex）仍按字面量编码为 `%25`。
    assert_eq!(canonicalize_uri("/a%zz"), "/a%25zz");
  }

  // ===== M30 错误分支单测：补 happy-path 之外的 error pathway 覆盖 =====

  #[tokio::test]
  async fn sign_returns_credential_invalid_when_no_credential() {
    let signer = fixed_signer();
    let req = build_send_sms_request();
    let (mut parts, _) = req.into_parts();
    let result = signer.sign_request(&Context::new(), &mut parts, None, None).await;
    assert!(result.is_err(), "missing credential should fail");
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
      err_msg.to_lowercase().contains("credential") || err_msg.to_lowercase().contains("required"),
      "error should mention credential: {err_msg}"
    );
    let _ = req; // suppress unused
  }

  #[tokio::test]
  async fn sign_rejects_non_ascii_header_value() {
    let cred = Credential::new("LTAItestkey", "testsecret");
    let signer = fixed_signer();
    // Insert a header with non-ASCII bytes — should fail in canonical headers
    // collection because to_str() returns InvalidHeaderValue.
    let mut req = Request::builder()
      .method("POST")
      .uri("https://dysmsapi.aliyuncs.com/?Action=Test")
      .header("x-acs-action", "Test")
      .header("x-acs-version", "2017-05-25")
      .header("x-acs-tag", http::HeaderValue::from_bytes(b"\xff\xfe").unwrap())
      .body(Bytes::new())
      .unwrap();
    let (mut parts, _) = std::mem::replace(&mut req, Request::new(Bytes::new())).into_parts();
    let result = signer.sign_request(&Context::new(), &mut parts, Some(&cred), None).await;
    assert!(result.is_err(), "non-ASCII header should fail signing");
    let err_msg = format!("{:?}", result.unwrap_err()).to_lowercase();
    assert!(err_msg.contains("ascii") || err_msg.contains("header"), "error should mention header/ASCII: {err_msg}");
  }

  #[tokio::test]
  async fn sign_handles_uri_without_authority() {
    // URI 仅 path（无 scheme + authority）—— host header 不会被自动注入，
    // canonical_request 用 caller 显式 host header（若提供）；不 panic。
    let cred = Credential::new("LTAItestkey", "testsecret");
    let signer = fixed_signer();
    let mut req = Request::builder()
      .method("POST")
      .uri("/relative-path?x=1")
      .header("host", "dysmsapi.aliyuncs.com")
      .header("x-acs-action", "Test")
      .body(Bytes::new())
      .unwrap();
    let (mut parts, _) = std::mem::replace(&mut req, Request::new(Bytes::new())).into_parts();
    // 不应 panic（之前 .uri.authority().host() 链路在 None 时被 if let 守护）
    let result = signer.sign_request(&Context::new(), &mut parts, Some(&cred), None).await;
    assert!(result.is_ok(), "URI without authority + explicit host header should sign OK: {result:?}");
    assert!(parts.headers.contains_key("authorization"), "Authorization header should be set");
  }

  #[test]
  fn canonicalize_query_handles_duplicate_keys() {
    // 阿里云规范：同名参数按字典序拼接 — 验证 sort 稳定且 key 重复时 value 也参与排序
    let canonical = canonicalize_query("k=2&k=1&a=z");
    // a=z 在前（按 key），k=1 / k=2 按 value 字典序
    assert_eq!(canonical, "a=z&k=1&k=2");
  }
}
