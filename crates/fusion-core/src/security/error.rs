use serde::Serialize;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
  #[error("Hmac failure new from slice")]
  HmacFailNewFromSlice,

  #[error("Invalid format")]
  InvalidFormat,

  #[error("Cannot decode ident")]
  CannotDecodeIdent,

  #[error("Cannot decode exp")]
  CannotDecodeExp,

  #[error("Signature not matching")]
  SignatureNotMatching,

  #[error("Exp not iso")]
  ExpNotIso,

  #[error("Token expired")]
  TokenExpired,

  #[error("Token not yet valid")]
  TokenNotYetValid,

  #[error("Failed to hash password")]
  FailedToHashPassword,

  #[error("Invalid password")]
  InvalidPassword,

  #[error("Failed to verify password")]
  FailedToVerifyPassword,

  /// `tokio::task::spawn_blocking` JoinError —— argon2 worker 线程异常
  /// （runtime shutdown / OOM / panic）。生产路径应 graceful 上报而非 panic。
  #[error("Password worker join failed: {0}")]
  PasswordWorkerJoinFailed(String),

  #[error(transparent)]
  JoseError(#[from] josekit::JoseError),
}

impl Serialize for Error {
  fn serialize<S>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    serializer.serialize_str(&self.to_string())
  }
}
