//! MailError：邮件投递错误分级（framework-conventions §1——上游瞬态 vs 上游永久
//! 拒绝 vs 本地缺陷，MUST NOT 整类压成单一变体；源错误链一律保留）。

/// 装箱的错误源（跨 feature 形态统一：SMTP 错误类型仅在 `smtp` feature 下存在）。
pub type BoxedMailError = Box<dyn std::error::Error + Send + Sync>;

/// 邮件投递错误。
#[derive(Debug, thiserror::Error)]
pub enum MailError {
  /// 上游瞬态失败（SMTP 4xx / 网络）——可重试。
  #[error("mail transient failure: {message}")]
  Transient {
    message: String,
    #[source]
    source: BoxedMailError,
  },
  /// 上游永久拒绝（SMTP 5xx：认证失败 / 拒收 / 域名无效）——重试无意义。
  #[error("mail permanent rejection: {message}")]
  Permanent {
    message: String,
    #[source]
    source: BoxedMailError,
  },
  /// 其他传输错误（未分级 / 信封构造失败）。
  #[error("mail transport error: {source}")]
  Transport {
    #[source]
    source: BoxedMailError,
  },
  /// 本地装配缺陷（feature 未启用 / TLS 参数构造失败）——重试无意义。
  #[error("mailer configuration error: {0}")]
  Config(String),
}

impl MailError {
  /// 瞬态失败判定（消费方据此决定是否重试）。
  pub fn is_transient(&self) -> bool {
    matches!(self, Self::Transient { .. })
  }

  /// 上游永久拒绝判定。
  pub fn is_permanent(&self) -> bool {
    matches!(self, Self::Permanent { .. })
  }

  /// 装箱任意传输错误（保留源链，不吞 `to_string()`）。
  pub fn transport(source: impl Into<BoxedMailError>) -> Self {
    Self::Transport { source: source.into() }
  }
}

#[cfg(feature = "smtp")]
impl From<lettre::transport::smtp::Error> for MailError {
  fn from(e: lettre::transport::smtp::Error) -> Self {
    let message = e.to_string();
    if e.is_transient() {
      Self::Transient { message, source: Box::new(e) }
    } else if e.is_permanent() {
      Self::Permanent { message, source: Box::new(e) }
    } else {
      Self::transport(e)
    }
  }
}
