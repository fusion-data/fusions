use base64ct::{Base64UrlUnpadded, Encoding};
pub use hmac::digest::InvalidLength;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::error::Error;

type HmacSha256 = Hmac<Sha256>;

pub fn hmac_sha256(secret: &[u8], data: &[u8]) -> Result<Vec<u8>, InvalidLength> {
  let mut mac = HmacSha256::new_from_slice(secret)?;
  mac.update(data);
  let result = mac.finalize().into_bytes().to_vec();
  Ok(result)
}

#[inline]
pub fn hmac_sha256_string(secret: &[u8], data: &[u8]) -> Result<String, InvalidLength> {
  let bytes = hmac_sha256(secret, data)?;
  Ok(base16ct::lower::encode_string(&bytes))
}

pub fn sha256(s: &[u8]) -> Vec<u8> {
  let mut hasher: Sha256 = Sha256::new();
  hasher.update(s);
  hasher.finalize().to_vec()
}

#[inline]
pub fn sha256_string(s: &[u8]) -> String {
  let result = sha256(s);
  base16ct::lower::encode_string(&result)
}

pub fn b64u_encode(content: impl AsRef<[u8]>) -> String {
  Base64UrlUnpadded::encode_string(content.as_ref())
}

pub fn b64u_decode(b64u: &str) -> Result<Vec<u8>, Error> {
  Base64UrlUnpadded::decode_vec(b64u).map_err(|_| Error::FailToB64uDecode(b64u.to_string()))
}

pub fn b64u_decode_to_string(b64u: &str) -> Result<String, Error> {
  // 区分两类失败：base64 非法 → FailToB64uDecode；解码后非 UTF-8 → B64uDecodedNotUtf8。
  let bytes = b64u_decode(b64u)?;
  String::from_utf8(bytes).map_err(|_| Error::B64uDecodedNotUtf8(b64u.to_string()))
}
