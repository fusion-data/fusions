//! 文件传输（feature `file`）：MIME 全文落盘（`.eml`），本地走查 / 调试面。

use lettre::{Transport as _, transport::file::FileTransport};

use crate::{MailError, MailSender, SentMessage};

/// 文件发送器：邮件以 MIME 全文写入指定目录，文件名含时间戳与序列。
pub struct FileSender {
  transport: FileTransport,
}

impl FileSender {
  /// 目录不存在则创建（FileTransport 落盘不建目录——机制层吸收，调用方零操心）。
  pub fn new(dir: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
    std::fs::create_dir_all(&dir)?;
    Ok(Self { transport: FileTransport::new(dir) })
  }
}

impl MailSender for FileSender {
  fn send(&self, message: &crate::Message) -> Result<SentMessage, MailError> {
    let id = self.transport.send(message).map_err(MailError::transport)?;
    Ok(SentMessage { code: 0, message: id.to_string() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use lettre::message::{Mailbox, Message, header::ContentType};

  #[test]
  fn file_sender_writes_eml() {
    let dir = std::env::temp_dir().join(format!("fusion-mail-test-{}", std::process::id()));
    let sender = FileSender::new(dir.clone()).expect("dir");
    let from: Mailbox = "sender@example.com".parse().expect("mailbox");
    let to: Mailbox = "recipient@example.com".parse().expect("mailbox");
    let message = Message::builder()
      .from(from)
      .to(to)
      .subject("file transport")
      .header(ContentType::TEXT_PLAIN)
      .body("hello".to_string())
      .expect("message");
    let sent = sender.send(&message).expect("written");
    let path = dir.join(format!("{}.eml", sent.message));
    let content = std::fs::read_to_string(&path).expect("eml readable");
    assert!(content.contains("Subject: file transport"), "subject missing: {content}");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(dir);
  }
}
