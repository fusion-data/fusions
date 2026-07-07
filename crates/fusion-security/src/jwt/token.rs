use fusion_common::ctx::CtxPayload;
use fusion_core::{configuration::SecuritySetting, security::SecurityUtils};

use crate::error::SecurityError;

pub fn make_token(sc: &SecuritySetting, payload: CtxPayload) -> Result<String, SecurityError> {
  let token = SecurityUtils::encrypt_jwt(sc.pwd(), payload).map_err(|_e| SecurityError::TokenGeneration)?;
  Ok(token)
}

pub fn make_token_by_user_id(sc: &SecuritySetting, uid: impl Into<String>) -> Result<String, SecurityError> {
  let mut payload = CtxPayload::default();
  payload.set_subject(uid);
  make_token(sc, payload)
}
