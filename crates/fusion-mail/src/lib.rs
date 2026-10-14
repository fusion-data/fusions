//! fusion-mail: 邮件发送机制层（SMTP / 文件 / 记录 / 丢弃传输）。
//!
//! 只收机制不收业务（framework-conventions §7）：
//! - [`MailSender`] / [`AsyncMailSender`]：窄发送 trait（同步 + 异步双形态），
//!   实现隐藏传输差异（深模块）。
//! - 消息构造直接用 lettre [`Message`] builder（多部分正文 / 附件 / 内嵌图 /
//!   自定义头全继承）；lettre 类型经本 crate re-export 引用，下游 MUST NOT
//!   自带 lettre 直接依赖——版本偏斜会产出 `expected Message, found Message`
//!   类似编译误诊（fusion-ai re-export 先例同构）。
//! - 错误分级：上游瞬态（可重试）/ 上游永久拒绝 / 本地装配缺陷三分
//!   （[`MailError`]，framework-conventions §1），源错误链一律保留。
//!
//! 留在消费方（crate 刻意不收）：邮件文案 / 模板 / i18n、场景方法（如验证邮件）、
//! 重试与告警策略、DKIM 密钥装配、配置来源（env / 配置中心）与装配分流。

mod config;
mod error;
#[cfg(feature = "file")]
mod file;
#[cfg(feature = "smtp")]
mod smtp;
mod stub;

pub use config::{SmtpConfig, TlsMode};
pub use error::{BoxedMailError, MailError};
#[cfg(feature = "file")]
pub use file::FileSender;
#[cfg(feature = "smtp")]
pub use smtp::{AsyncSmtpSender, SmtpSender};
pub use stub::{RecordedMessage, RecordingSender, StubSender};

// —— lettre 透传（版本偏斜防护：下游经 fusion_mail:: 引用，不自带 lettre 依赖）——
pub use lettre::message;
pub use lettre::message::{Mailbox, Message};

/// 发送结果摘要（窄接口：消费方无需理解 lettre Response 结构）。
#[derive(Debug, Clone)]
pub struct SentMessage {
  /// 服务器应答码（SMTP 2xx = 接受；文件 / 记录 / 丢弃传输为 0 或 250 占位）。
  pub code: u16,
  /// 应答消息（SMTP 服务器消息通常含 queue id；文件传输为落盘路径）。
  pub message: String,
}

/// 同步发送器。
pub trait MailSender: Send + Sync {
  /// 发送一封邮件。
  fn send(&self, message: &Message) -> Result<SentMessage, MailError>;
}

/// 异步发送器（tokio1）。
#[cfg(feature = "tokio1")]
#[async_trait::async_trait]
pub trait AsyncMailSender: Send + Sync {
  /// 发送一封邮件（Message 按值传递——上游异步 transport 所有权形态）。
  async fn send(&self, message: Message) -> Result<SentMessage, MailError>;
}

// —— 工厂（未启用 feature 时返回指向 feature 的错误——区别于「不支持」，
//    防排障误导；fusion-storage build_operator 先例同构）——

/// 构造同步 SMTP 发送器（feature `smtp`）。
#[cfg(feature = "smtp")]
pub fn build_smtp_sender(config: &SmtpConfig) -> Result<SmtpSender, MailError> {
  SmtpSender::new(config)
}

/// 构造异步 SMTP 发送器（feature `smtp` + `tokio1`）。
#[cfg(all(feature = "smtp", feature = "tokio1"))]
pub fn build_async_smtp_sender(config: &SmtpConfig) -> Result<AsyncSmtpSender, MailError> {
  AsyncSmtpSender::new(config)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::error::Error as _;

  /// 错误链保留验证：Transport 变体 source() 非 None
  /// （framework-conventions §1——MUST NOT 只留 to_string()）。
  #[test]
  fn mail_error_preserves_source() {
    let source = std::io::Error::other("connection reset");
    let err = MailError::transport(source);
    assert!(err.source().is_some(), "error chain lost");
    assert!(!err.is_transient());
    assert!(!err.is_permanent());
  }

  #[test]
  fn mail_error_variants_classify() {
    let transient =
      MailError::Transient { message: "421 busy".to_string(), source: Box::new(std::io::Error::other("retry later")) };
    assert!(transient.is_transient());
    let permanent = MailError::Permanent {
      message: "535 auth failed".to_string(),
      source: Box::new(std::io::Error::other("bad credentials")),
    };
    assert!(permanent.is_permanent());
    assert!(permanent.to_string().contains("permanent rejection"));
  }
}
