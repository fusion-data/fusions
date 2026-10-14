//! 记录与丢弃传输（恒可用，无 feature gate——零外联依赖的最小测试面）。

use std::sync::Mutex;

use crate::{MailError, MailSender, SentMessage};

/// 已发送消息的快照（envelope + 提取头 + MIME 全文）。
#[derive(Debug, Clone)]
pub struct RecordedMessage {
  /// 信封发件人（reverse path；退信地址，可能与头 From 不同）。
  pub from: Option<String>,
  /// 信封收件人列表（forward path）。
  pub to: Vec<String>,
  /// 提取的 Subject 头（未设则为 None）。
  pub subject: Option<String>,
  /// MIME 全文（兜底断言面：正文 / 自定义头 contains 断言）。
  pub raw: Vec<u8>,
}

/// 记录型发送器（测试 / 调试）：保存已发送消息快照供断言，不实际投递。
///
/// 与 [`StubSender`] 的分工：记录即本类型的行为（命名如实）；丢弃归 stub。
pub struct RecordingSender {
  sent: Mutex<Vec<RecordedMessage>>,
}

impl Default for RecordingSender {
  fn default() -> Self {
    Self::new()
  }
}

impl RecordingSender {
  pub fn new() -> Self {
    Self { sent: Mutex::new(Vec::new()) }
  }

  /// 已发送快照（克隆返回——断言侧持锁零交叉）。
  pub fn sent(&self) -> Vec<RecordedMessage> {
    self.sent.lock().expect("recording sender lock poisoned").clone()
  }

  /// 清空记录（测试隔离用）。
  pub fn clear(&self) {
    self.sent.lock().expect("recording sender lock poisoned").clear();
  }
}

impl MailSender for RecordingSender {
  fn send(&self, message: &crate::Message) -> Result<SentMessage, MailError> {
    self.sent.lock().expect("recording sender lock poisoned").push(record(message));
    // code = 250（SMTP 成功应答形态——消费方按 2xx 判定；文件传输的 0 形态见 file.rs）。
    Ok(SentMessage { code: 250, message: "recorded".to_string() })
  }
}

#[cfg(feature = "tokio1")]
#[async_trait::async_trait]
impl crate::AsyncMailSender for RecordingSender {
  async fn send(&self, message: crate::Message) -> Result<SentMessage, MailError> {
    MailSender::send(self, &message)
  }
}

/// 丢弃型发送器：丢弃邮件恒成功（lettre stub 语义透传——联通性走查 / 冒烟用）。
pub struct StubSender;

impl MailSender for StubSender {
  fn send(&self, _message: &crate::Message) -> Result<SentMessage, MailError> {
    Ok(SentMessage { code: 250, message: "dropped".to_string() })
  }
}

#[cfg(feature = "tokio1")]
#[async_trait::async_trait]
impl crate::AsyncMailSender for StubSender {
  async fn send(&self, _message: crate::Message) -> Result<SentMessage, MailError> {
    MailSender::send(self, &_message)
  }
}

/// 从 Message 提取快照（envelope 面 + Subject 头 + MIME 全文）。
fn record(message: &crate::Message) -> RecordedMessage {
  let envelope = message.envelope();
  let subject = message.headers().get::<lettre::message::header::Subject>().map(|s| s.as_ref().to_string());
  RecordedMessage {
    from: envelope.from().map(|a| a.to_string()),
    to: envelope.to().iter().map(|a| a.to_string()).collect(),
    subject,
    raw: message.formatted(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use lettre::message::{Mailbox, Message, header::ContentType};

  fn message() -> Message {
    let from: Mailbox = "sender@example.com".parse().expect("mailbox");
    let to: Mailbox = "recipient@example.com".parse().expect("mailbox");
    Message::builder()
      .from(from)
      .to(to)
      .subject("hello")
      .header(ContentType::TEXT_PLAIN)
      .body("body text".to_string())
      .expect("message")
  }

  #[test]
  fn recording_sender_captures_envelope_and_subject() {
    let sender = RecordingSender::new();
    sender.send(&message()).expect("recorded");
    let sent = sender.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec!["recipient@example.com".to_string()]);
    assert_eq!(sent[0].from.as_deref(), Some("sender@example.com"));
    assert_eq!(sent[0].subject.as_deref(), Some("hello"));
    let raw = String::from_utf8(sent[0].raw.clone()).expect("utf8");
    assert!(raw.contains("body text"), "raw body missing: {raw}");
  }

  #[test]
  fn stub_sender_always_succeeds() {
    let sender = StubSender;
    let sent = sender.send(&message()).expect("stub send");
    assert_eq!(sent.code, 250);
    assert_eq!(sent.message, "dropped");
  }
}
