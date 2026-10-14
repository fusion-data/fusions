//! SmtpConfig：SMTP 连接配置（持密脱敏 Debug——framework-conventions §2）。

use lettre::message::Mailbox;

/// TLS 握手形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
  /// 明文（仅可信内网中继）。
  Plain,
  /// STARTTLS 升级（端口 587 常见）。
  #[default]
  StartTls,
  /// 隐式 TLS 直连（端口 465 常见）。
  Wrapper,
}

impl TlsMode {
  /// 该形态的常见端口（配置缺省值参考，非硬约束）。
  pub fn common_port(self) -> u16 {
    match self {
      Self::Plain => 25,
      Self::StartTls => 587,
      Self::Wrapper => 465,
    }
  }

  /// 按端口号推断握手形态（465 → 直连，其余 → STARTTLS；25 明文需显式声明）。
  pub fn from_port(port: u16) -> Self {
    if port == 465 { Self::Wrapper } else { Self::StartTls }
  }
}

/// SMTP 发送配置。
///
/// 凭据字段（`password`）在 Debug 输出中脱敏——`tracing::debug!(?config)` 会把
/// 派生 Debug 的明文密钥落盘（framework-conventions §2 判据）。
#[derive(Clone)]
pub struct SmtpConfig {
  pub host: String,
  pub port: u16,
  pub tls: TlsMode,
  pub username: String,
  pub password: String,
  pub from: Mailbox,
}

impl std::fmt::Debug for SmtpConfig {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SmtpConfig")
      .field("host", &self.host)
      .field("port", &self.port)
      .field("tls", &self.tls)
      .field("username", &self.username)
      .field("password", &"<REDACTED>")
      .field("from", &self.from)
      .finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> SmtpConfig {
    SmtpConfig {
      host: "smtp.example.com".to_string(),
      port: 465,
      tls: TlsMode::Wrapper,
      username: "no-reply@example.com".to_string(),
      password: "secret-token-value".to_string(),
      from: "no-reply@example.com".parse().expect("mailbox"),
    }
  }

  /// framework-conventions §2 验证锚点：Debug 输出 MUST NOT 泄漏密码明文，
  /// 且脱敏字段可辨识（区别于「字段缺失」的静默形态）。
  #[test]
  fn smtp_config_debug_never_leaks_password() {
    let debug = format!("{:?}", sample());
    assert!(!debug.contains("secret-token-value"), "debug leaked password: {debug}");
    assert!(debug.contains("<REDACTED>"), "redaction marker missing: {debug}");
  }

  #[test]
  fn tls_mode_port_inference_round_trip() {
    assert_eq!(TlsMode::from_port(465), TlsMode::Wrapper);
    assert_eq!(TlsMode::from_port(587), TlsMode::StartTls);
    assert_eq!(TlsMode::from_port(TlsMode::Wrapper.common_port()), TlsMode::Wrapper);
    assert_eq!(TlsMode::from_port(TlsMode::StartTls.common_port()), TlsMode::StartTls);
  }
}
