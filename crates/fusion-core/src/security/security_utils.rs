use std::time::{Duration, SystemTime};

use fusion_common::ctx::CtxPayload;
use josekit::{jwe::JweHeader, jwt::JwtPayload};

use crate::configuration::KeyConf;

use super::{
  Error,
  jose::{decrypt_jwe_dir, encrypt_jwe_dir},
};

/// Clock-skew tolerance applied to JWT temporal claims (`exp` / `nbf`).
///
/// Without leeway a token signed on one host can be rejected as expired (or
/// not-yet-valid) on another whose wall clock drifts by a few seconds — a
/// common occurrence across a fleet even with NTP. 60s is the de-facto
/// industry default (matches `jsonwebtoken`, Auth0, and RFC 7519 §4.1.4's
/// "small leeway" guidance).
const CLOCK_LEEWAY: Duration = Duration::from_secs(60);

pub struct SecurityUtils;

impl SecurityUtils {
  pub fn encrypt_jwt(key_conf: &dyn KeyConf, payload: CtxPayload) -> Result<String, Error> {
    let payload = JwtPayload::from_map(payload.into_inner())?;
    Self::encrypt_jwt_payload(key_conf, payload)
  }

  pub fn encrypt_jwt_payload(key_conf: &dyn KeyConf, mut payload: JwtPayload) -> Result<String, Error> {
    if payload.expires_at().is_none() {
      let expires_at = key_conf.expires_at().into();
      payload.set_expires_at(&expires_at);
    }

    encrypt_jwe_dir(key_conf.secret_key(), &payload).map_err(Error::JoseError)
  }

  pub fn decrypt_jwt(key_conf: &dyn KeyConf, token: &str) -> Result<(CtxPayload, JweHeader), Error> {
    let (payload, header) = Self::decrypt_jwt_payload(key_conf, token)?;
    let payload = CtxPayload::new(payload.into());
    Ok((payload, header))
  }

  pub fn decrypt_jwt_payload(key_conf: &dyn KeyConf, token: &str) -> Result<(JwtPayload, JweHeader), Error> {
    let (payload, header) = decrypt_jwe_dir(key_conf.secret_key(), token)?;
    let now = SystemTime::now();

    // `exp`: reject only once expiry is more than CLOCK_LEEWAY in the past,
    // i.e. `expires_at + leeway < now`.
    if let Some(expires_at) = payload.expires_at()
      && expires_at + CLOCK_LEEWAY < now
    {
      return Err(Error::TokenExpired);
    }

    // `nbf`: reject only when not-before is more than CLOCK_LEEWAY in the
    // future, i.e. `nbf - leeway > now`. A token whose nbf is within leeway
    // of now is accepted to absorb clock skew between signer and verifier.
    if let Some(not_before) = payload.not_before()
      && not_before > now + CLOCK_LEEWAY
    {
      return Err(Error::TokenNotYetValid);
    }

    Ok((payload, header))
  }
}
#[cfg(test)]
mod tests {
  use std::time::{Duration, SystemTime};

  use fusion_common::ctx::CtxPayload;

  use crate::configuration::TokenConf;

  use super::*;

  fn make_key_conf() -> TokenConf {
    TokenConf {
      secret_key: b"0123456789ABCDEF0123456789ABCDEF".to_vec(),
      expires_in: 7200,
      public_key: br#"-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEOTv4YquENmDfXoSN0TQiOqmgR1Px
UDTicuyW06VcX/XOkXp/6vmIIBFUXVWREJmQy7EIhNXM1qCy7Hs6SK9y7A==
-----END PUBLIC KEY-----"#
        .to_vec(),
      private_key: br#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgbMlaUVhOz9IHvlxT
4i7Wm6cmubzzGZr/PNNME25ZVNuhRANCAAQ5O/hiq4Q2YN9ehI3RNCI6qaBHU/FQ
NOJy7JbTpVxf9c6Ren/q+YggEVRdVZEQmZDLsQiE1czWoLLsezpIr3Ls
-----END PRIVATE KEY-----"#
        .to_vec(),
    }
  }

  #[test]
  fn test_encrypt_and_decrypt_jwt() {
    let key_conf = make_key_conf();
    let mut payload = CtxPayload::default();
    payload.set_subject("test_user");
    payload.set_exp(
      (SystemTime::now() + Duration::from_secs(600))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64,
    );

    let token = SecurityUtils::encrypt_jwt(&key_conf, payload.clone()).expect("encrypt_jwt failed");
    let (decrypted_payload, _header) = SecurityUtils::decrypt_jwt(&key_conf, &token).expect("decrypt_jwt failed");

    assert_eq!(decrypted_payload.get_subject(), payload.get_subject());
    assert!(decrypted_payload.get_exp().is_some());
  }

  #[test]
  fn test_token_expired() {
    let mut key_conf = make_key_conf();
    key_conf.expires_in = -10;
    let mut payload = CtxPayload::default();
    payload.set_subject("expired_user");
    // Past the 60s clock leeway — must be rejected.
    payload.set_exp(
      (SystemTime::now() - Duration::from_secs(120))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64,
    );

    let token = SecurityUtils::encrypt_jwt(&key_conf, payload).expect("encrypt_jwt failed");
    let result = SecurityUtils::decrypt_jwt(&key_conf, &token);

    assert!(matches!(result, Err(Error::TokenExpired)));
  }

  #[test]
  fn test_token_within_leeway_accepted() {
    let key_conf = make_key_conf();
    let mut payload = CtxPayload::default();
    payload.set_subject("skewed_user");
    // Expired 20s ago — within the 60s leeway, so still accepted (simulates
    // a slightly fast verifier clock).
    payload.set_exp(
      (SystemTime::now() - Duration::from_secs(20))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64,
    );

    let token = SecurityUtils::encrypt_jwt(&key_conf, payload).expect("encrypt_jwt failed");
    let result = SecurityUtils::decrypt_jwt(&key_conf, &token);

    assert!(result.is_ok(), "token within clock leeway should be accepted");
  }

  #[test]
  fn test_token_not_yet_valid() {
    let key_conf = make_key_conf();
    let mut payload = JwtPayload::new();
    payload.set_subject("future_user");
    // nbf far in the future, well past the 60s leeway — must be rejected.
    payload.set_not_before(&(SystemTime::now() + Duration::from_secs(600)));
    payload.set_expires_at(&(SystemTime::now() + Duration::from_secs(1200)));

    let token = SecurityUtils::encrypt_jwt_payload(&key_conf, payload).expect("encrypt failed");
    let result = SecurityUtils::decrypt_jwt(&key_conf, &token);

    assert!(matches!(result, Err(Error::TokenNotYetValid)));
  }

  #[test]
  fn test_token_nbf_within_leeway_accepted() {
    let key_conf = make_key_conf();
    let mut payload = JwtPayload::new();
    payload.set_subject("near_future_user");
    // nbf 20s in the future — within leeway, accepted.
    payload.set_not_before(&(SystemTime::now() + Duration::from_secs(20)));
    payload.set_expires_at(&(SystemTime::now() + Duration::from_secs(600)));

    let token = SecurityUtils::encrypt_jwt_payload(&key_conf, payload).expect("encrypt failed");
    let result = SecurityUtils::decrypt_jwt(&key_conf, &token);

    assert!(result.is_ok(), "nbf within clock leeway should be accepted");
  }
}
