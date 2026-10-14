//! SMTP 传输（feature `smtp`）：lettre SmtpTransport + rustls TLS + 连接池。
//!
//! 构造一次、发送多次（lettre `pool` feature 连接复用——per-request 重建
//! TCP+TLS 握手是 fusion AGENTS「HTTP client 复用」同源判据的邮件版）。

use lettre::{
  AsyncSmtpTransport, Tokio1Executor, Transport as _,
  transport::smtp::{SmtpTransport, authentication::Credentials, client::Tls, client::TlsParameters},
};

#[cfg(feature = "tokio1")]
use lettre::AsyncTransport as _;

use crate::{MailError, MailSender, SentMessage, SmtpConfig, TlsMode};

/// 按 TLS 形态组装底层 transport（凭证恒设——主流 SMTP 服务商均要求认证）。
fn build_transport(config: &SmtpConfig) -> Result<SmtpTransport, MailError> {
  let mut builder = SmtpTransport::builder_dangerous(&config.host).port(config.port);
  builder = match config.tls {
    TlsMode::Plain => builder,
    TlsMode::StartTls | TlsMode::Wrapper => {
      let params = TlsParameters::new(config.host.clone())
        .map_err(|e| MailError::Config(format!("tls parameters for {}: {e}", config.host)))?;
      let tls = if config.tls == TlsMode::Wrapper { Tls::Wrapper(params) } else { Tls::Required(params) };
      builder.tls(tls)
    }
  };
  Ok(builder.credentials(Credentials::new(config.username.clone(), config.password.clone())).build())
}

/// 同步 SMTP 发送器。
///
/// `send` 阻塞至 SMTP 会话完成（含 TCP+TLS 握手与投递）；async 服务内的低频
/// 发送可直接调用，高频面消费方自行 `spawn_blocking` / 改用 [`AsyncSmtpSender`]。
pub struct SmtpSender {
  transport: SmtpTransport,
}

impl SmtpSender {
  pub fn new(config: &SmtpConfig) -> Result<Self, MailError> {
    Ok(Self { transport: build_transport(config)? })
  }
}

impl MailSender for SmtpSender {
  fn send(&self, message: &crate::Message) -> Result<SentMessage, MailError> {
    let response = self.transport.send(message).map_err(MailError::from)?;
    let message_text = response.message().collect::<Vec<_>>().join(" ");
    tracing::debug!(code = u16::from(response.code()), "fusion_mail: smtp message accepted");
    Ok(SentMessage { code: u16::from(response.code()), message: message_text })
  }
}

/// 异步 SMTP 发送器（tokio1）。
pub struct AsyncSmtpSender {
  transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl AsyncSmtpSender {
  pub fn new(config: &SmtpConfig) -> Result<Self, MailError> {
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host).port(config.port);
    builder = match config.tls {
      TlsMode::Plain => builder,
      TlsMode::StartTls | TlsMode::Wrapper => {
        let params = TlsParameters::new(config.host.clone())
          .map_err(|e| MailError::Config(format!("tls parameters for {}: {e}", config.host)))?;
        let tls = if config.tls == TlsMode::Wrapper { Tls::Wrapper(params) } else { Tls::Required(params) };
        builder.tls(tls)
      }
    };
    Ok(Self {
      transport: builder.credentials(Credentials::new(config.username.clone(), config.password.clone())).build(),
    })
  }
}

#[cfg(feature = "tokio1")]
#[async_trait::async_trait]
impl crate::AsyncMailSender for AsyncSmtpSender {
  async fn send(&self, message: crate::Message) -> Result<SentMessage, MailError> {
    let response = self.transport.send(message).await.map_err(MailError::from)?;
    let message_text = response.message().collect::<Vec<_>>().join(" ");
    tracing::debug!(code = u16::from(response.code()), "fusion_mail: smtp message accepted");
    Ok(SentMessage { code: u16::from(response.code()), message: message_text })
  }
}
