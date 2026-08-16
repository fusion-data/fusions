//! 语音合成（TTS）协议客户端公共底座：错误模型、流式音频块、SSE 分帧、
//! WAV 封装与音频容器探测。
//!
//! 供应商协议客户端（`providers::{dashscope, minimax, volcengine}`）共用这里的
//! 原语，方法签名形态对齐：`clone_voice` / `synthesize`（整段）/
//! `synthesize_stream`（`AudioPart` 流）。不在此定义统一 trait——各家请求参数
//! 差异大（情感枚举 / 语气指令 / Resource-Id 路由属调用方决策），鸭子对齐即可。

use bytes::Bytes;

/// 语音协议客户端统一错误。
///
/// 分级语义与 [`crate::providers::openai_compatible::OpenAiCompatError`] 同源：
/// `is_retryable()` 为 true 的错误（限流 / 上游 5xx / 传输层抖动）调用方应退避
/// 重试；`Protocol` / `RequestBuild` 是本地缺陷，重试无意义。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpeechError {
  /// provider 返回非 2xx（body 原样进 message，供应商错误码已含在其中）。
  #[error("upstream HTTP error ({status}): {message}")]
  Http { status: u16, message: String },

  /// HTTP 200 内的业务错误（MiniMax `base_resp.status_code` / 豆包流内 8 位码）。
  /// `rate_limited` / `transient` 由各家错误码表判定（客户端构造点给出）。
  #[error("vendor error ({code}): {message}")]
  Vendor { code: String, message: String, rate_limited: bool, transient: bool },

  /// 连接层失败（DNS / 连接拒绝 / 超时）。
  #[error("transport error: {0}")]
  Transport(String),

  /// 响应解析失败 / 缺必需字段（重试无意义）。
  #[error("protocol error: {0}")]
  Protocol(String),

  /// 请求构造失败（参数校验不过等）。
  #[error("request build error: {0}")]
  RequestBuild(String),

  /// 流式响应中途断开（网络抖动，可重试）。
  #[error("stream midway error: {0}")]
  StreamMidway(String),
}

impl SpeechError {
  pub(crate) fn request_build(msg: impl Into<String>) -> Self {
    Self::RequestBuild(msg.into())
  }

  pub(crate) fn protocol(msg: impl Into<String>) -> Self {
    Self::Protocol(msg.into())
  }

  /// 限流错误（调用方应指数退避，而非立即失败）。
  pub fn is_rate_limited(&self) -> bool {
    match self {
      Self::Http { status, .. } => *status == 429,
      Self::Vendor { rate_limited, .. } => *rate_limited,
      _ => false,
    }
  }

  /// 是否可重试：限流 / 上游瞬态（5xx、业务码 transient）/ 传输层与流中途错误。
  pub fn is_retryable(&self) -> bool {
    match self {
      Self::Transport(_) | Self::StreamMidway(_) => true,
      Self::Http { status, .. } => *status == 429 || *status >= 500,
      Self::Vendor { rate_limited, transient, .. } => *rate_limited || *transient,
      Self::Protocol(_) | Self::RequestBuild(_) => false,
    }
  }
}

impl From<reqwest::Error> for SpeechError {
  fn from(err: reqwest::Error) -> Self {
    Self::Transport(err.to_string())
  }
}

/// 流式音频块（客户端统一产出形态，`is_last` 在客户端内消化各家终态判据：
/// MiniMax `extra_info` / 豆包 `code=20000000` 结束行 / DashScope 末块 data 空）。
#[derive(Debug, Clone)]
pub struct AudioPart {
  pub bytes: Bytes,
  pub is_last: bool,
}

/// SSE `data:` payload 分帧器。
///
/// 喂入任意切块的字节流，吐出已完整的 `data:` 行 payload（UTF-8 lossy）。
/// 忽略注释（`:` 开头）、`event:` 行与空行；`data:` 前缀剥离后 trim。
/// MiniMax T2A 与 DashScope Qwen-TTS 的 SSE 都是纯 `data:` 行形态，共用本分帧器。
#[derive(Debug, Default)]
pub struct SseDataParser {
  buffer: String,
}

impl SseDataParser {
  pub fn new() -> Self {
    Self::default()
  }

  /// 喂入一块响应字节，返回其中完整 SSE data 行的 payload 列表（残行留缓冲）。
  pub fn push_chunk(&mut self, bytes: &[u8]) -> Vec<String> {
    self.buffer.push_str(&String::from_utf8_lossy(bytes));
    let mut payloads = Vec::new();
    while let Some(pos) = self.buffer.find('\n') {
      let line: String = self.buffer.drain(..=pos).collect();
      let line = line.trim_end_matches(['\n', '\r']);
      if let Some(payload) = line.strip_prefix("data:") {
        let payload = payload.trim();
        if !payload.is_empty() {
          payloads.push(payload.to_string());
        }
      }
    }
    payloads
  }
}

/// PCM 数据的 RIFF/WAV 头（44 字节标准头，无 LIST 附加块）。
///
/// `data_len = None` 写 0xFFFFFFFF 占位——流式场景总长未知，浏览器按 EOF 收尾
/// 容忍该值（校准点：真实联调时确认目标播放器行为）。
pub fn pcm_wav_header(sample_rate: u32, channels: u16, bits_per_sample: u16, data_len: Option<usize>) -> Vec<u8> {
  let data_len = data_len.map_or(0xFFFF_FFFF, |n| n as u32);
  let byte_rate = sample_rate * channels as u32 * (bits_per_sample / 8) as u32;
  let block_align = channels * (bits_per_sample / 8);
  let mut h = Vec::with_capacity(44);
  h.extend_from_slice(b"RIFF");
  // data_len 占位 0xFFFFFFFF 时 36 + len 溢出，saturating 保持「巨大值」语义
  h.extend_from_slice(&36u32.saturating_add(data_len).to_le_bytes()); // ChunkSize
  h.extend_from_slice(b"WAVE");
  h.extend_from_slice(b"fmt ");
  h.extend_from_slice(&16u32.to_le_bytes()); // Subchunk1Size (PCM)
  h.extend_from_slice(&1u16.to_le_bytes()); // AudioFormat = PCM
  h.extend_from_slice(&channels.to_le_bytes());
  h.extend_from_slice(&sample_rate.to_le_bytes());
  h.extend_from_slice(&byte_rate.to_le_bytes());
  h.extend_from_slice(&block_align.to_le_bytes());
  h.extend_from_slice(&bits_per_sample.to_le_bytes());
  h.extend_from_slice(b"data");
  h.extend_from_slice(&data_len.to_le_bytes());
  h
}

/// 音频容器探测（magic bytes）。
///
/// 覆盖本底座三家客户端的上传样本与合成产物格式；探测不出按 `Mp3` 兜底
/// （存量业务样本以 mp3 为主）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioContainer {
  Mp3,
  Wav,
  Ogg,
  M4a,
}

impl AudioContainer {
  /// data URI / multipart 用的 MIME。
  pub fn mime(self) -> &'static str {
    match self {
      Self::Mp3 => "audio/mpeg",
      Self::Wav => "audio/wav",
      Self::Ogg => "audio/ogg",
      Self::M4a => "audio/mp4",
    }
  }

  /// 豆包 unidirectional `audio_params.format` 等短名形态。
  pub fn format_name(self) -> &'static str {
    match self {
      Self::Mp3 => "mp3",
      Self::Wav => "wav",
      Self::Ogg => "ogg",
      Self::M4a => "m4a",
    }
  }
}

/// 按 magic bytes 探测音频容器。
pub fn detect_audio_container(bytes: &[u8]) -> AudioContainer {
  if bytes.len() > 3 && &bytes[0..3] == b"ID3" {
    return AudioContainer::Mp3;
  }
  if bytes.len() > 1 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0 {
    return AudioContainer::Mp3;
  }
  if bytes.len() > 3 && &bytes[0..4] == b"RIFF" {
    return AudioContainer::Wav;
  }
  if bytes.len() > 3 && &bytes[0..4] == b"OggS" {
    return AudioContainer::Ogg;
  }
  if bytes.len() > 7 && &bytes[4..8] == b"ftyp" {
    return AudioContainer::M4a;
  }
  AudioContainer::Mp3
}

/// hex 编码（MiniMax 音频以 hex 串往返）。
pub fn encode_hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// hex 解码（长度非偶 / 非法字符返 `None`）。
pub fn decode_hex(hex: &str) -> Option<Vec<u8>> {
  let hex = hex.trim();
  if !hex.len().is_multiple_of(2) {
    return None;
  }
  (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn speech_error_retry_classification() {
    // 限流：429 / Vendor rate_limited
    assert!(SpeechError::Http { status: 429, message: String::new() }.is_rate_limited());
    assert!(SpeechError::Http { status: 429, message: String::new() }.is_retryable());
    assert!(
      SpeechError::Vendor { code: "1002".into(), message: String::new(), rate_limited: true, transient: false }
        .is_rate_limited()
    );
    // 上游瞬态：5xx / Vendor transient / 传输层
    assert!(SpeechError::Http { status: 503, message: String::new() }.is_retryable());
    assert!(!SpeechError::Http { status: 503, message: String::new() }.is_rate_limited());
    assert!(
      SpeechError::Vendor { code: "55000000".into(), message: String::new(), rate_limited: false, transient: true }
        .is_retryable()
    );
    assert!(SpeechError::Transport("connect refused".into()).is_retryable());
    assert!(SpeechError::StreamMidway("eof".into()).is_retryable());
    // 本地缺陷：4xx / Protocol / RequestBuild
    assert!(!SpeechError::Http { status: 401, message: String::new() }.is_retryable());
    assert!(
      !SpeechError::Vendor { code: "2013".into(), message: String::new(), rate_limited: false, transient: false }
        .is_retryable()
    );
    assert!(!SpeechError::Protocol("bad json".into()).is_retryable());
    assert!(!SpeechError::RequestBuild("empty text".into()).is_retryable());
  }

  #[test]
  fn sse_parser_frames_data_lines_across_chunks() {
    let mut p = SseDataParser::new();
    // 一块内含多条 + 残行
    let out = p.push_chunk(b"data: {\"a\":1}\n: comment\nevent: x\ndata: {\"b\"");
    assert_eq!(out, vec![r#"{"a":1}"#]);
    let out = p.push_chunk(b":2}\n\ndata: [DONE]\n");
    assert_eq!(out, vec![r#"{"b":2}"#, "[DONE]"]);
    // data: 无空格形态
    let out = p.push_chunk(b"data:{\"c\":3}\n");
    assert_eq!(out, vec![r#"{"c":3}"#]);
  }

  #[test]
  fn wav_header_fields() {
    // 24kHz / 16bit / mono，44 字节标准头
    let h = pcm_wav_header(24_000, 1, 16, Some(4800));
    assert_eq!(h.len(), 44);
    assert_eq!(&h[0..4], b"RIFF");
    assert_eq!(&h[8..12], b"WAVE");
    assert_eq!(&h[36..40], b"data");
    assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 4800);
    assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 4800);
    assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 24_000); // sample_rate
    assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 48_000); // byte_rate
    // 流式占位
    let h = pcm_wav_header(24_000, 1, 16, None);
    assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 0xFFFF_FFFF);
  }

  #[test]
  fn detect_audio_container_by_magic() {
    assert_eq!(detect_audio_container(b"ID3\x04\x00tag"), AudioContainer::Mp3);
    assert_eq!(detect_audio_container(&[0xFF, 0xFB, 0x90, 0x00]), AudioContainer::Mp3);
    assert_eq!(detect_audio_container(b"RIFFxxxxWAVE"), AudioContainer::Wav);
    assert_eq!(detect_audio_container(b"OggSxxxx"), AudioContainer::Ogg);
    assert_eq!(detect_audio_container(b"xxxxftypM4A "), AudioContainer::M4a);
    assert_eq!(detect_audio_container(b"\x00\x01\x02"), AudioContainer::Mp3, "兜底 mp3");
  }

  #[test]
  fn hex_roundtrip() {
    assert_eq!(encode_hex(&[0xDE, 0xAD, 0xBE, 0xEF]), "deadbeef");
    assert_eq!(decode_hex("deadbeef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(decode_hex("abc"), None, "奇数长度");
    assert_eq!(decode_hex("zz"), None, "非法字符");
    assert_eq!(decode_hex(" deadbeef ").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
  }
}
