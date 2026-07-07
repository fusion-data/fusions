//! 通用流式语音识别(STT)抽象。
//!
//! 区别于 [`rig::transcription::TranscriptionModel`](rig 的批量文件转写,
//! 走单次 HTTP `multipart/form-data` 调用),本模块面向**双向流 / 长连接**的实时
//! STT 协议(WebSocket / gRPC streaming),适配阿里云 Paraformer 实时 v2、
//! OpenAI Realtime transcription、讯飞实时转写、sherpa-onnx 等。
//!
//! 设计参考 `docs/designs/ai/voice-ai-tech-design.md` 附录 A。

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

/// 音频编码格式。流式 STT 通常只支持 PCM/Opus,文件 batch 接口接受更多格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioEncoding {
  PcmS16Le,
  PcmF32Le,
  G711Ulaw,
  G711Alaw,
  Opus,
  WebmOpus,
  Wav,
  Mp3,
  Aac,
  Amr,
}

impl AudioEncoding {
  /// Provider 通常约定的字面名,如阿里 Paraformer `format` 字段。
  pub fn as_provider_str(self) -> &'static str {
    match self {
      Self::PcmS16Le | Self::PcmF32Le => "pcm",
      Self::G711Ulaw => "g711_ulaw",
      Self::G711Alaw => "g711_alaw",
      Self::Opus | Self::WebmOpus => "opus",
      Self::Wav => "wav",
      Self::Mp3 => "mp3",
      Self::Aac => "aac",
      Self::Amr => "amr",
    }
  }
}

/// 实时 STT 会话的音频流配置。
#[derive(Debug, Clone)]
pub struct AudioStreamConfig {
  pub encoding: AudioEncoding,
  pub sample_rate: u32,
  pub channels: u16,
  /// 单帧时长(ms);PCM s16le 16k mono 推荐 40ms。0 表示由 provider 默认。
  pub frame_duration_ms: u16,
  /// 语言候选,如 `["zh", "en"]`。
  pub language_hints: Vec<String>,
  /// 热词列表(姓名 / 床号 / 医疗术语)。
  pub hotwords: Vec<String>,
  /// 业务领域提示,例如 `"healthcare_nursing"`。
  pub domain_hint: Option<String>,
  /// Provider 特有扩展参数(走 `serde_json::Value` 透传)。
  pub provider_options: serde_json::Value,
}

impl AudioStreamConfig {
  /// PCM s16le 16k mono 40ms frame 默认配置,匹配浏览器 AudioWorklet 链路。
  pub fn pcm_s16le_16k_mono_40ms() -> Self {
    Self {
      encoding: AudioEncoding::PcmS16Le,
      sample_rate: 16_000,
      channels: 1,
      frame_duration_ms: 40,
      language_hints: vec!["zh".to_string(), "en".to_string()],
      hotwords: Vec::new(),
      domain_hint: None,
      provider_options: serde_json::Value::Null,
    }
  }
}

/// 单段转录结果(对应阿里 result-generated 的一个 sentence)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
  pub text: String,
  /// 段内相对开始时间(ms)。
  pub begin_ms: Option<u64>,
  /// 段内相对结束时间(ms);流式中间结果可能为 None。
  pub end_ms: Option<u64>,
  /// 段级置信度(provider 提供时填入)。
  pub confidence: Option<f32>,
  /// 词级切分(provider 提供时填入)。
  pub words: Vec<TranscriptWord>,
  /// 是否为该段的最终(`sentence_end=true`)。
  pub is_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptWord {
  pub text: String,
  pub begin_ms: Option<u64>,
  pub end_ms: Option<u64>,
  pub punctuation: Option<String>,
}

/// 一次会话的最终转录结果(任务结束时合成)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResult {
  /// 拼接后的完整文本。
  pub text: String,
  pub language: Option<String>,
  pub confidence: Option<f32>,
  pub segments: Vec<TranscriptSegment>,
  /// Provider 短名(`paraformer_realtime_v2` / `openai_realtime_transcription` 等)。
  pub provider: String,
  /// Provider 会话 ID(task_id / session_id),便于审计追踪。
  pub provider_session_id: Option<String>,
}

/// 流式 STT 事件。
#[derive(Debug, Clone)]
pub enum SttEvent {
  /// 服务端确认 session 建立,可以推音频帧。
  Started { provider_session_id: Option<String> },
  /// 增量识别结果(`sentence_end=false`)。
  Partial(TranscriptSegment),
  /// 段最终结果(`sentence_end=true`,可能后续仍有新段)。
  SegmentFinal(TranscriptSegment),
  /// 任务结束的合并结果。
  TaskFinished(TranscriptionResult),
  /// Provider 端错误,事件流随后通常会被服务端关闭。
  Error { code: String, message: String, retryable: bool },
}

/// 流式 STT 错误。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpeechToTextError {
  #[error("invalid config: {0}")]
  ConfigInvalid(String),

  #[error("auth failed: {0}")]
  Auth(String),

  #[error("network: {0}")]
  Network(String),

  #[error("protocol: {0}")]
  Protocol(String),

  #[error("provider {provider} returned {code}: {message}")]
  Provider { provider: String, code: String, message: String, retryable: bool },

  #[error("timeout: {0}")]
  Timeout(String),

  #[error("cancelled")]
  Cancelled,

  #[error(transparent)]
  Other(#[from] anyhow::Error),
}

impl SpeechToTextError {
  /// 调用方据此决定是否换 provider 重放。
  pub fn is_retryable(&self) -> bool {
    matches!(self, Self::Network(_) | Self::Timeout(_) | Self::Provider { retryable: true, .. })
  }
}

/// 音频帧流的别名:每个 `Bytes` 为一帧 PCM/Opus 数据,顺序到达。
pub type AudioFrameStream = Pin<Box<dyn Stream<Item = Bytes> + Send>>;

/// STT 事件流的别名。
pub type SttEventStream = Pin<Box<dyn Stream<Item = Result<SttEvent, SpeechToTextError>> + Send>>;

/// 通用流式 STT 抽象。Provider 实现按需选择是否支持批量。
#[async_trait]
pub trait SpeechToText: Send + Sync {
  /// Provider 短名,用于审计 / 指标标签,如 `"paraformer_realtime_v2"`。
  fn provider_name(&self) -> &'static str;

  /// 启动一次实时识别 session。
  ///
  /// 调用方提供音频帧流(顺序、有限或无限),返回 STT 事件流。
  /// 实现负责:鉴权握手、控制消息、帧编码、断流时关闭上游。
  async fn transcribe_realtime(
    &self,
    audio: AudioFrameStream,
    config: AudioStreamConfig,
  ) -> Result<SttEventStream, SpeechToTextError>;

  /// 可选的批量文件接口(默认未实现,只为 OpenAI Whisper 等 batch-only provider 留口)。
  async fn transcribe_batch(
    &self,
    _audio: Bytes,
    _config: AudioStreamConfig,
  ) -> Result<TranscriptionResult, SpeechToTextError> {
    Err(SpeechToTextError::ConfigInvalid("batch transcription not supported".into()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_pcm_config_matches_browser_audio_worklet() {
    let cfg = AudioStreamConfig::pcm_s16le_16k_mono_40ms();
    assert_eq!(cfg.sample_rate, 16_000);
    assert_eq!(cfg.channels, 1);
    assert_eq!(cfg.frame_duration_ms, 40);
    assert_eq!(cfg.encoding.as_provider_str(), "pcm");
  }

  #[test]
  fn retryable_classification() {
    assert!(SpeechToTextError::Network("disconnect".into()).is_retryable());
    assert!(SpeechToTextError::Timeout("rtt".into()).is_retryable());
    assert!(
      SpeechToTextError::Provider {
        provider: "x".into(),
        code: "rate_limit".into(),
        message: "throttle".into(),
        retryable: true,
      }
      .is_retryable()
    );
    assert!(!SpeechToTextError::Auth("bad key".into()).is_retryable());
    assert!(
      !SpeechToTextError::Provider {
        provider: "x".into(),
        code: "invalid".into(),
        message: "bad param".into(),
        retryable: false,
      }
      .is_retryable()
    );
  }
}
