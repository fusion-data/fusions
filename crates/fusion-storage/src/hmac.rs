//! HMAC-SHA256 签名/验证（fs 后端本地路由兜底用）。
//!
//! 机制层：密钥全部显式参数化——env 装配、默认密钥、进程级缓存归消费方。
//! 云后端走 opendal native presign，不经过 HMAC。
//!
//! 签名消息格式（wire 契约，MUST NOT 变更——签发与验签两侧跨版本互认）：
//! - 读：`{creator_id}:{key}:{expires}`
//! - 上传：`PUT:{creator_id}:{key}:{expires}`
//! - 常量时间比较（`mac.verify_slice`），防时序攻击。

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 计算 HMAC-SHA256，返回 hex 编码 token。
fn hmac_sign(message: &str, hmac_secret: &[u8]) -> String {
  let mut mac = HmacSha256::new_from_slice(hmac_secret).expect("HMAC key length is valid");
  mac.update(message.as_bytes());
  hex::encode(mac.finalize().into_bytes())
}

/// 常量时间验证 HMAC token。
fn hmac_verify(message: &str, token: &str, hmac_secret: &[u8]) -> bool {
  let mut mac = HmacSha256::new_from_slice(hmac_secret).expect("HMAC key length is valid");
  mac.update(message.as_bytes());
  match hex::decode(token) {
    Ok(token_bytes) => mac.verify_slice(&token_bytes).is_ok(),
    Err(_) => false,
  }
}

/// 验证读 URL 的 HMAC 签名（fs 后端本地路由用）。
///
/// 签名消息：`{creator_id}:{key}:{expires}`。先查过期，再验签。
pub fn verify_hmac(key: &str, expires: i64, token: &str, creator_id: &str, hmac_secret: &[u8]) -> bool {
  if chrono::Utc::now().timestamp() > expires {
    return false;
  }
  hmac_verify(&format!("{creator_id}:{key}:{expires}"), token, hmac_secret)
}

/// 验证上传 URL 的 HMAC 签名（fs 后端本地路由用）。
///
/// 签名消息：`PUT:{creator_id}:{key}:{expires}`。
pub fn verify_upload_hmac(key: &str, expires: i64, token: &str, creator_id: &str, hmac_secret: &[u8]) -> bool {
  if chrono::Utc::now().timestamp() > expires {
    return false;
  }
  hmac_verify(&format!("PUT:{creator_id}:{key}:{expires}"), token, hmac_secret)
}

/// 生成读 URL 的 HMAC 签名（fs 后端本地路由用）。
pub fn sign_read(key: &str, expires: i64, creator_id: &str, hmac_secret: &[u8]) -> String {
  hmac_sign(&format!("{creator_id}:{key}:{expires}"), hmac_secret)
}

/// 生成上传 URL 的 HMAC 签名（fs 后端本地路由用）。
pub fn sign_upload(key: &str, expires: i64, creator_id: &str, hmac_secret: &[u8]) -> String {
  hmac_sign(&format!("PUT:{creator_id}:{key}:{expires}"), hmac_secret)
}

#[cfg(test)]
mod tests {
  use super::*;

  const TEST_SECRET: &[u8] = b"test-secret-key";

  // 金样对拍锚（从抽取前消费仓实现独立计算）：同输入必须产出逐字节相同签名。
  // 输入 = (secret "golden-secret", creator "creator-1", key "creator-1/knowledge/asset-1",
  // expires 4102444800)。
  const GOLDEN_READ_TOKEN: &str = "d9da5de5c0962fdf7c290b00257a026b216cee83912e21bb3f8c22b1826b0ea1";
  const GOLDEN_UPLOAD_TOKEN: &str = "d5da3d8d851dc4263421343ee21eb0d2b4a01fa2682fc2178c6cd376998b7b22";
  const GOLDEN_SECRET: &[u8] = b"golden-secret";
  const GOLDEN_CREATOR: &str = "creator-1";
  const GOLDEN_KEY: &str = "creator-1/knowledge/asset-1";
  const GOLDEN_EXPIRES: i64 = 4102444800;

  #[test]
  fn read_sign_verify_roundtrip() {
    let key = "creator-1/knowledge/asset-1";
    let creator_id = "creator-1";
    let expires = chrono::Utc::now().timestamp() + 900;
    let token = sign_read(key, expires, creator_id, TEST_SECRET);
    assert!(verify_hmac(key, expires, &token, creator_id, TEST_SECRET));
  }

  #[test]
  fn upload_sign_verify_roundtrip() {
    let key = "creator-1/voice-samples/s1/file.wav";
    let creator_id = "creator-1";
    let expires = chrono::Utc::now().timestamp() + 900;
    let token = sign_upload(key, expires, creator_id, TEST_SECRET);
    assert!(verify_upload_hmac(key, expires, &token, creator_id, TEST_SECRET));
  }

  #[test]
  fn verify_rejects_expired() {
    let key = "k";
    let creator_id = "c";
    let expires = chrono::Utc::now().timestamp() - 1; // 已过期
    let token = sign_read(key, expires, creator_id, TEST_SECRET);
    assert!(!verify_hmac(key, expires, &token, creator_id, TEST_SECRET));
  }

  #[test]
  fn verify_rejects_wrong_creator() {
    let key = "k";
    let expires = chrono::Utc::now().timestamp() + 900;
    let token = sign_read(key, expires, "creator-1", TEST_SECRET);
    // 用 creator-2 验证 → 失败
    assert!(!verify_hmac(key, expires, &token, "creator-2", TEST_SECRET));
  }

  #[test]
  fn verify_rejects_tampered_key() {
    let creator_id = "c";
    let expires = chrono::Utc::now().timestamp() + 900;
    let token = sign_read("original-key", expires, creator_id, TEST_SECRET);
    assert!(!verify_hmac("tampered-key", expires, &token, creator_id, TEST_SECRET));
  }

  /// 金样对拍：读签名与抽取前实现逐字节一致（wire 契约跨版本锁定）。
  #[test]
  fn read_sign_matches_pre_extraction_golden() {
    assert_eq!(sign_read(GOLDEN_KEY, GOLDEN_EXPIRES, GOLDEN_CREATOR, GOLDEN_SECRET), GOLDEN_READ_TOKEN);
    // 过期时间戳固定在未来（4102444800 = 2100-01-01），验签方向同样成立
    assert!(verify_hmac(GOLDEN_KEY, GOLDEN_EXPIRES, GOLDEN_READ_TOKEN, GOLDEN_CREATOR, GOLDEN_SECRET));
  }

  /// 金样对拍：上传签名与抽取前实现逐字节一致。
  #[test]
  fn upload_sign_matches_pre_extraction_golden() {
    assert_eq!(sign_upload(GOLDEN_KEY, GOLDEN_EXPIRES, GOLDEN_CREATOR, GOLDEN_SECRET), GOLDEN_UPLOAD_TOKEN);
    assert!(verify_upload_hmac(GOLDEN_KEY, GOLDEN_EXPIRES, GOLDEN_UPLOAD_TOKEN, GOLDEN_CREATOR, GOLDEN_SECRET));
  }

  /// 非 hex token 不 panic、直接拒绝（hex::decode 失败路径）。
  #[test]
  fn verify_rejects_non_hex_token() {
    assert!(!verify_hmac("k", chrono::Utc::now().timestamp() + 900, "not-hex!", "c", TEST_SECRET));
  }
}
