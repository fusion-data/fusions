//! 豆包语音 **V3**（openspeech.bytedance.com）：声音复刻 + 单向流式合成。
//!
//! 协议（单凭证 `X-Api-Key`）：
//! - 复刻：`POST /api/v3/tts/voice_clone`（单接口 JSON：base64 样本 +
//!   `custom_speaker_id` 自定义代号 + `extra_params.demo_text` 试听文案）。
//!   后付费音色训练免槽位费，首次正式合成才计费——试听 MUST 走响应 `demo_audio`
//!   （训练产物，base64）。
//! - 合成：`POST /api/v3/tts/unidirectional`（HTTP chunked **JSON 行流**：
//!   `data` 为 base64 音频块、`code=20000000` 为成功结束行）。`X-Api-Resource-Id`
//!   按音色来源路由（`ResourceId` 枚举）；复刻音色（ICL）还需 `req_params.model`
//!   显式指定 tts 系枚举（`seed-tts-2.0-standard` 等，非 seed-icl-2.0 系）。
//!
//! 音色代号（custom_speaker_id）由调用方生成传入（命名规范校验在 vendor 侧：
//! 8-256 字符、字母开头、`[a-zA-Z0-9_-]`、避开保留前缀）。
//!
//! 参考：docs.volcengine.com/docs/6561（豆包语音 V3 HTTP 接口）

use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use serde::Deserialize;

use crate::providers::speech::{AudioPart, SpeechError, detect_audio_container};

/// 声音复刻 V3 音色训练端点（单接口：上传样本 + 训练 + 返回 demo 试听）。
const VOICE_CLONE_PATH: &str = "/api/v3/tts/voice_clone";
/// 单向流式合成 V3 端点（一次输入文本，chunked JSON 行流式返回）。
const TTS_UNIDIRECTIONAL_PATH: &str = "/api/v3/tts/unidirectional";
/// 流式成功结束行 code。
const STREAM_DONE_CODE: i64 = 20_000_000;
/// 后付费音色训练时 `speaker_id` 的固定占位值（实际代号在 `custom_speaker_id`）。
const CUSTOM_SLOT_PLACEHOLDER: &str = "custom_speaker_id";

/// `X-Api-Resource-Id` 按音色来源路由（协议层枚举；「哪个 profile 用哪个
/// Resource-Id」的判据是调用方业务知识——如按音色代号前缀识别克隆产物）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceId {
  /// 语音合成大模型 2.0（控制台预置音色）。
  Tts,
  /// 声音复刻大模型 2.0（声音复刻接口克隆的音色）。
  Icl,
}

impl ResourceId {
  fn as_str(self) -> &'static str {
    match self {
      Self::Tts => "seed-tts-2.0",
      Self::Icl => "seed-icl-2.0",
    }
  }
}

/// 豆包语音 V3 客户端（复刻 + 合成）。
///
/// 持 `api_key`，按 framework-conventions §2 手写脱敏 Debug（MUST NOT derive）。
pub struct DoubaoSpeech {
  api_key: Option<String>,
  base_url: String,
  http: reqwest::Client,
}

impl std::fmt::Debug for DoubaoSpeech {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DoubaoSpeech")
      .field("api_key", &self.api_key.as_ref().map(|_| "<REDACTED>"))
      .field("base_url", &self.base_url)
      .finish_non_exhaustive()
  }
}

impl DoubaoSpeech {
  /// 豆包语音默认端点。
  pub const DEFAULT_BASE_URL: &'static str = "https://openspeech.bytedance.com";

  pub fn new(api_key: Option<String>) -> Self {
    Self::with_base_url(api_key, Self::DEFAULT_BASE_URL.to_string())
  }

  /// 从环境变量构造（`DOUBAO_SPEECH_API_KEY` + 可选 `DOUBAO_SPEECH_BASE_URL`）。
  pub fn from_env() -> Self {
    let key = std::env::var("DOUBAO_SPEECH_API_KEY").ok().filter(|s| !s.is_empty());
    let base_url = std::env::var("DOUBAO_SPEECH_BASE_URL")
      .ok()
      .filter(|s| !s.is_empty())
      .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());
    Self::with_base_url(key, base_url)
  }

  /// 带 base_url 构造（测试指向 mock server）。
  pub fn with_base_url(api_key: Option<String>, base_url: String) -> Self {
    Self {
      api_key,
      base_url,
      http: reqwest::Client::builder().timeout(Duration::from_secs(300)).build().expect("reqwest client build"),
    }
  }

  pub fn is_configured(&self) -> bool {
    self.api_key.is_some()
  }

  fn require_config(&self) -> Result<&str, SpeechError> {
    self
      .api_key
      .as_deref()
      .ok_or_else(|| SpeechError::request_build("doubao speech api_key missing (DOUBAO_SPEECH_API_KEY)"))
  }

  /// 每请求唯一 `X-Api-Request-Id`（官方要求 uuid；v4 hex 形态）。
  fn request_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
  }

  fn post(&self, path: &str, resource_id: Option<ResourceId>, body: &serde_json::Value) -> reqwest::RequestBuilder {
    let mut builder = self
      .http
      .post(format!("{}{path}", self.base_url))
      .header("X-Api-Key", self.api_key.as_deref().unwrap_or_default())
      .header("X-Api-Request-Id", Self::request_id());
    if let Some(rid) = resource_id {
      builder = builder.header("X-Api-Resource-Id", rid.as_str());
    }
    builder.json(body)
  }

  async fn check_http(response: reqwest::Response) -> Result<reqwest::Response, SpeechError> {
    let status = response.status();
    if !status.is_success() {
      let body = response.text().await.unwrap_or_default();
      return Err(SpeechError::Http { status: status.as_u16(), message: body });
    }
    Ok(response)
  }

  // =====================================================================
  // 声音复刻（单接口）
  // =====================================================================

  /// 声音复刻：样本 bytes + 调用方生成的音色代号 → 训练产物（含 demo 试听）。
  ///
  /// `demo_text`：训练自带的试听文案（4-300 字）。后付费音色试听不触发「转正」
  /// 计费——试听 MUST 用返回的 `demo_audio`，MUST NOT 回落正式合成。
  pub async fn clone_voice(
    &self,
    sample_audio: &[u8],
    custom_speaker_id: &str,
    demo_text: &str,
  ) -> Result<DoubaoClonedVoice, SpeechError> {
    let _key = self.require_config()?;
    if sample_audio.is_empty() {
      return Err(SpeechError::request_build("clone_voice sample audio is empty"));
    }
    if custom_speaker_id.is_empty() {
      return Err(SpeechError::request_build("clone_voice custom_speaker_id is empty"));
    }
    let body = serde_json::json!({
      "speaker_id": CUSTOM_SLOT_PLACEHOLDER,
      "custom_speaker_id": custom_speaker_id,
      "audio": {
        // format 对 mp3/ogg/wav/aac 可省略，显式传更稳（magic bytes 探测）
        "data": base64::engine::general_purpose::STANDARD.encode(sample_audio),
        "format": detect_audio_container(sample_audio).format_name(),
      },
      // 0 = 中文
      "language": 0,
      "extra_params": { "demo_text": demo_text },
    });
    let response = self.post(VOICE_CLONE_PATH, None, &body).send().await.map_err(SpeechError::from)?;
    let response = Self::check_http(response).await?;
    let parsed: CloneVoiceResponse = response
      .json()
      .await
      .map_err(|e| SpeechError::protocol(format!("parse voice_clone response: {e}")))?;
    // status: 2 Success / 4 Active 可调 TTS；0 NotFound / 1 Training / 3 Failed
    // 均为异常终态/中间态（接口语义为同步训练完成，不应返回 Training）。
    match parsed.status {
      Some(2 | 4) => {}
      other => {
        return Err(SpeechError::Vendor {
          code: format!("voice_status_{other:?}"),
          message: parsed.message.unwrap_or_default(),
          rate_limited: false,
          transient: false,
        });
      }
    }
    // demo_audio 编码形态官方未明示（按 base64 处理）：解码失败降级 None，
    // 调用方回落自行合成预览并经日志暴露缺口——clone 本身成功，不因试听作废。
    let demo_audio = match parsed.demo_audio.as_deref() {
      Some(d) if !d.is_empty() => {
        base64::engine::general_purpose::STANDARD.decode(d).map(Bytes::from).map(Some).unwrap_or_else(|_| {
          tracing::warn!(
            target: "fusion_ai::providers::volcengine",
            len = d.len(),
            "voice_clone demo_audio is not valid base64, caller falls back to synthesize preview"
          );
          None
        })
      }
      _ => None,
    };
    Ok(DoubaoClonedVoice { speaker_id: custom_speaker_id.to_string(), demo_audio })
  }

  // =====================================================================
  // 合成（chunked JSON 行流）
  // =====================================================================

  /// 整段合成（= 收集全部流式 data 块拼接）。
  pub async fn synthesize(&self, req: UnidirectionalRequest<'_>) -> Result<Bytes, SpeechError> {
    let mut stream = self.synthesize_stream(req).await?;
    let mut audio = Vec::new();
    while let Some(part) = stream.next().await {
      audio.extend_from_slice(&part?.bytes);
    }
    if audio.is_empty() {
      return Err(SpeechError::protocol("tts/unidirectional returned empty audio"));
    }
    Ok(Bytes::from(audio))
  }

  /// 流式合成：JSON 行流（块边界与行边界不对齐，内部行缓冲）；`code=20000000`
  /// 结束行之后的最后一块标 `is_last`（pending 预读模式）。
  pub async fn synthesize_stream(
    &self,
    req: UnidirectionalRequest<'_>,
  ) -> Result<futures::stream::BoxStream<'static, Result<AudioPart, SpeechError>>, SpeechError> {
    self.require_config()?;
    if req.text.is_empty() {
      return Err(SpeechError::request_build("synthesize text is empty"));
    }
    let response = self
      .post(TTS_UNIDIRECTIONAL_PATH, Some(req.resource_id), &unidirectional_body(&req))
      .send()
      .await
      .map_err(SpeechError::from)?;
    let response = Self::check_http(response).await?;

    let byte_stream = response.bytes_stream();
    let stream = async_stream::try_stream! {
      let mut byte_stream = std::pin::pin!(byte_stream);
      let mut buffer = String::new();
      let mut pending: Option<Vec<u8>> = None;
      let mut done = false;
      while !done {
        let chunk = match byte_stream.next().await {
          Some(c) => c.map_err(|e| SpeechError::StreamMidway(e.to_string()))?,
          None => {
            // 流结束：处理无结尾换行的残行（Done / 错误行常是 body 最后一行）
            if !buffer.trim().is_empty()
              && let Some(event) = parse_stream_line(buffer.trim())
            {
              match event {
                StreamEvent::Data(audio) => {
                  if let Some(prev) = pending.take() {
                    yield AudioPart { bytes: Bytes::from(prev), is_last: false };
                  }
                  pending = Some(audio);
                }
                StreamEvent::Done => {}
                StreamEvent::Error { code, message } => Err(classify_stream_code(code, &message))?,
                StreamEvent::Ignore => {}
              }
            }
            break;
          }
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find('\n') {
          let line: String = buffer.drain(..=pos).collect();
          let line = line.trim();
          if line.is_empty() {
            continue;
          }
          match parse_stream_line(line) {
            Some(StreamEvent::Data(audio)) => {
              if let Some(prev) = pending.take() {
                yield AudioPart { bytes: Bytes::from(prev), is_last: false };
              }
              pending = Some(audio);
            }
            Some(StreamEvent::Done) => {
              done = true;
              break;
            }
            Some(StreamEvent::Error { code, message }) => {
              Err(classify_stream_code(code, &message))?
            }
            Some(StreamEvent::Ignore) | None => {}
          }
        }
      }
      if let Some(last) = pending.take() {
        yield AudioPart { bytes: Bytes::from(last), is_last: true };
      }
    };
    Ok(stream.boxed())
  }
}

/// 声音复刻请求（最终协议参数形态）。
#[derive(Debug, Clone, Copy)]
pub struct UnidirectionalRequest<'a> {
  pub text: &'a str,
  /// 音色：调用方克隆产物代号或控制台预置音色。
  pub speaker: &'a str,
  /// Resource-Id 路由（预置 → [`ResourceId::Tts`]；克隆 → [`ResourceId::Icl`]）。
  pub resource_id: ResourceId,
  /// 音频格式（"mp3" / "pcm" / "wav"）。
  pub format: &'a str,
  /// 语速倍率（1.0 = 原速；映射 `speech_rate` ∈ [-50,100]，0.5x-2.0x 线性）。
  pub speed: f32,
  /// 音量倍率（1.0 = 原速标度；映射 `loudness_rate`）。
  pub volume: f32,
  /// 复刻音色需显式指定的 tts 系 model 枚举（`seed-tts-2.0-standard` 等）；
  /// 预置音色 None。
  pub model: Option<&'a str>,
  /// 语气指令（豆包 2.0 的自然语言情感控制形态；复刻音色不支持，调用方决策）。
  pub context_texts: Option<&'a [String]>,
}

impl<'a> UnidirectionalRequest<'a> {
  pub fn new(text: &'a str, speaker: &'a str, resource_id: ResourceId) -> Self {
    Self { text, speaker, resource_id, format: "mp3", speed: 1.0, volume: 1.0, model: None, context_texts: None }
  }
}

/// 声音复刻产物。
#[derive(Debug, Clone)]
pub struct DoubaoClonedVoice {
  /// 调用方传入的音色代号（后续合成 `speaker` 参数）。
  pub speaker_id: String,
  /// 训练自带试听（base64 已解码；试听走此通道不触发首次合成计费）。
  /// None = 响应未携带 / 解码失败（调用方回落自行合成预览并承担计费语义）。
  pub demo_audio: Option<Bytes>,
}

fn unidirectional_body(req: &UnidirectionalRequest<'_>) -> serde_json::Value {
  let mut req_params = serde_json::json!({
    "text": req.text,
    "speaker": req.speaker,
    "audio_params": {
      "format": req.format,
      "speech_rate": scale_to_rate(req.speed),
      "loudness_rate": scale_to_rate(req.volume),
    },
  });
  if let Some(model) = req.model {
    req_params["model"] = serde_json::Value::String(model.to_string());
  }
  if let Some(context_texts) = req.context_texts
    && !context_texts.is_empty()
  {
    req_params["context_texts"] =
      serde_json::Value::Array(context_texts.iter().map(|t| serde_json::Value::String(t.clone())).collect());
  }
  serde_json::json!({ "req_params": req_params })
}

/// 语速/音量倍率（1.0 = 原速）→ `*_rate` 标度（0 = 原速，100 = 2.0x，-50 = 0.5x）。
fn scale_to_rate(v: f32) -> i32 {
  ((v - 1.0) * 100.0).clamp(-50.0, 100.0) as i32
}

// =========================================================================
// 流式 JSON 行解析
// =========================================================================

/// V3 音色训练响应。
#[derive(Debug, Deserialize)]
struct CloneVoiceResponse {
  #[serde(default)]
  message: Option<String>,
  #[serde(default)]
  status: Option<i32>,
  #[serde(default)]
  demo_audio: Option<String>,
}

/// 流式 JSON 行事件。
enum StreamEvent {
  /// `data` base64 音频块。
  Data(Vec<u8>),
  /// `code=20000000` 成功结束行。
  Done,
  /// 非 0 且非结束码的业务错误行。
  Error { code: i64, message: String },
  /// sentence 时间戳 / usage 计费等本实现不消费的行。
  Ignore,
}

/// 解析单行流式响应；非 JSON / 无关注行返回 None 或 Ignore。
fn parse_stream_line(line: &str) -> Option<StreamEvent> {
  let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
  let code = parsed.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
  let data = parsed.get("data").and_then(|d| d.as_str()).unwrap_or("");
  if !data.is_empty() {
    let audio = match base64::engine::general_purpose::STANDARD.decode(data) {
      Ok(a) => a,
      Err(_) => {
        tracing::warn!(
          target: "fusion_ai::providers::volcengine",
          "stream data is not valid base64, skipped"
        );
        Vec::new()
      }
    };
    if !audio.is_empty() {
      return Some(StreamEvent::Data(audio));
    }
  }
  if code == STREAM_DONE_CODE {
    return Some(StreamEvent::Done);
  }
  if code != 0 {
    let message = parsed.get("message").and_then(|m| m.as_str()).unwrap_or_default().to_string();
    return Some(StreamEvent::Error { code, message });
  }
  Some(StreamEvent::Ignore)
}

/// 流内业务错误码分类（8 位：4 开头客户错、5 开头服务端可重试；45000000 的
/// quota/concurrency 子形态是并发超限 → 限流退避重试）。
fn classify_stream_code(code: i64, message: &str) -> SpeechError {
  SpeechError::Vendor {
    code: code.to_string(),
    message: message.to_string(),
    rate_limited: code == 45_000_000 && (message.contains("quota") || message.contains("concurrency")),
    transient: (50_000_000..60_000_000).contains(&code),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn client(server_url: String) -> DoubaoSpeech {
    DoubaoSpeech::with_base_url(Some("test-key".into()), server_url)
  }

  fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
  }

  fn stream_body(chunks: &[&[u8]]) -> String {
    let mut lines: Vec<String> = chunks.iter().map(|c| format!(r#"{{"code":0,"data":"{}"}}"#, b64(c))).collect();
    lines.push(format!(r#"{{"code":{},"message":"ok","data":null}}"#, STREAM_DONE_CODE));
    lines.join("\n")
  }

  #[tokio::test]
  async fn synthesize_collects_stream_chunks_with_headers() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path(TTS_UNIDIRECTIONAL_PATH))
      .and(wiremock::matchers::header("x-api-key", "test-key"))
      .and(wiremock::matchers::header("x-api-resource-id", "seed-tts-2.0"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({
        "req_params": {
          "text": "你好世界",
          "speaker": "zh_male_1",
          "audio_params": { "format": "mp3", "speech_rate": 0, "loudness_rate": 0 },
        }
      })))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(stream_body(&[b"aaabbb", b"ccc"])))
      .expect(1)
      .mount(&server)
      .await;

    let audio = client(server.uri())
      .synthesize(UnidirectionalRequest::new("你好世界", "zh_male_1", ResourceId::Tts))
      .await
      .unwrap();
    assert_eq!(&audio[..], b"aaabbbccc");
  }

  #[tokio::test]
  async fn icl_request_carries_model_and_context_texts() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::header("x-api-resource-id", "seed-icl-2.0"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({
        "req_params": {
          "speaker": "hetu_1a2b3c4d5e6f",
          "model": "seed-tts-2.0-standard",
          "context_texts": ["请用开心的语气朗读"],
          "audio_params": { "speech_rate": 100, "loudness_rate": -50 },
        }
      })))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(stream_body(&[b"a"])))
      .expect(1)
      .mount(&server)
      .await;

    let req = UnidirectionalRequest {
      speed: 2.0,
      volume: 0.5,
      model: Some("seed-tts-2.0-standard"),
      context_texts: Some(&["请用开心的语气朗读".to_string()]),
      ..UnidirectionalRequest::new("t", "hetu_1a2b3c4d5e6f", ResourceId::Icl)
    };
    client(server.uri()).synthesize(req).await.unwrap();
  }

  #[tokio::test]
  async fn synthesize_stream_marks_last_chunk_after_done_line() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(stream_body(&[b"aa", b"bb"])))
      .mount(&server)
      .await;
    let mut stream = client(server.uri())
      .synthesize_stream(UnidirectionalRequest::new("t", "v", ResourceId::Tts))
      .await
      .unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(&first.bytes[..], b"aa");
    assert!(!first.is_last);
    let last = stream.next().await.unwrap().unwrap();
    assert_eq!(&last.bytes[..], b"bb");
    assert!(last.is_last);
    assert!(stream.next().await.is_none());
  }

  #[tokio::test]
  async fn stream_error_lines_and_http_errors_classified() {
    // 流内 45000000 + concurrency → rate limited；55000000 → transient
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .set_body_string(r#"{"code":45000000,"message":"quota exceeded for types: concurrency"}"#),
      )
      .mount(&server)
      .await;
    let err = client(server.uri())
      .synthesize(UnidirectionalRequest::new("t", "v", ResourceId::Tts))
      .await
      .unwrap_err();
    assert!(err.is_rate_limited(), "{err:?}");

    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(r#"{"code":55000000,"message":"internal"}"#))
      .mount(&server)
      .await;
    let err = client(server.uri())
      .synthesize(UnidirectionalRequest::new("t", "v", ResourceId::Tts))
      .await
      .unwrap_err();
    assert!(err.is_retryable(), "{err:?}");

    // HTTP 500 → transient；400 → permanent
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(500).set_body_string(r#"{"code":55000000,"message":"x"}"#))
      .mount(&server)
      .await;
    let err = client(server.uri())
      .synthesize(UnidirectionalRequest::new("t", "v", ResourceId::Tts))
      .await
      .unwrap_err();
    assert!(err.is_retryable(), "{err:?}");

    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(400).set_body_string(r#"{"code":45001001,"message":"bad param"}"#))
      .mount(&server)
      .await;
    let err = client(server.uri())
      .synthesize(UnidirectionalRequest::new("t", "v", ResourceId::Tts))
      .await
      .unwrap_err();
    assert!(!err.is_retryable(), "{err:?}");
  }

  #[tokio::test]
  async fn clone_voice_sends_base64_sample_and_parses_demo() {
    let sample = b"ID3\x03fake-mp3";
    let demo = b64(b"demo-pcm");
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path(VOICE_CLONE_PATH))
      .and(wiremock::matchers::header("x-api-key", "test-key"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({
        "speaker_id": "custom_speaker_id",
        "custom_speaker_id": "hetu_1a2b3c4d5e6f",
        "audio": { "format": "mp3" },
        "extra_params": { "demo_text": "试听文案" },
      })))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .set_body_json(serde_json::json!({ "status": 2, "message": "Success", "demo_audio": demo })),
      )
      .expect(1)
      .mount(&server)
      .await;

    let cloned = client(server.uri()).clone_voice(sample, "hetu_1a2b3c4d5e6f", "试听文案").await.unwrap();
    assert_eq!(cloned.speaker_id, "hetu_1a2b3c4d5e6f");
    assert_eq!(&cloned.demo_audio.unwrap()[..], b"demo-pcm");

    // 请求体里音频 base64 与 format 探测
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["audio"]["data"], b64(sample));
    assert_eq!(body["audio"]["format"], "mp3");
  }

  #[tokio::test]
  async fn clone_voice_abnormal_status_is_vendor_error() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .set_body_json(serde_json::json!({ "status": 3, "message": "Training Failed" })),
      )
      .mount(&server)
      .await;
    let err = client(server.uri()).clone_voice(b"ID3x", "hetu_x", "t").await.unwrap_err();
    assert!(!err.is_retryable(), "{err:?}");
  }

  #[test]
  fn debug_never_leaks_api_key() {
    let c = DoubaoSpeech::new(Some("secret-key".into()));
    let dbg = format!("{c:?}");
    assert!(!dbg.contains("secret-key"), "api_key leaked: {dbg}");
    assert!(dbg.contains("<REDACTED>"));
  }

  #[test]
  fn unconfigured_client_rejects_calls() {
    assert!(!DoubaoSpeech::new(None).is_configured());
    assert!(matches!(DoubaoSpeech::new(None).require_config(), Err(SpeechError::RequestBuild(_))));
  }
}
