//! 阿里云 DashScope **Qwen-TTS** 非实时语音合成（HTTP + SSE 流式）。
//!
//! 协议：`POST .../aigc/multimodal-generation/generation`，`model=qwen3-tts-flash`
//! 或声音复刻模型（`qwen3-tts-vc-*`），`input.voice` 为系统音色（"Cherry" 等）或
//! enrollment 返回的克隆音色名。加 `X-DashScope-SSE: enable` 头后逐段返回
//! base64 音频块——**首块自带 44 字节 WAV 头**（24kHz/16bit/mono，RIFF/data
//! size 为 0x7FFFFFFE 流式占位），次块起为裸 PCM，末块 `data` 为空、`url` 为
//! 完整音频 OSS 地址（24h 有效）。非流式直接返回 `output.audio.url`。
//!
//! 千问-TTS 系单请求文本上限 600 字符（按输入字符计费）：`synthesize` 超限自动
//! 按句切分多段合成、直拼后回写准确 RIFF/data size；`synthesize_stream` 同样
//! 支持分段顺序流式（段间串行，首段即出）。
//!
//! 参考：<https://help.aliyun.com/zh/model-studio/qwen-tts-api>、
//! <https://help.aliyun.com/zh/model-studio/non-realtime-tts-user-guide>

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;

use crate::providers::dashscope::{DashScopeCredentials, DashScopeRegion};
use crate::providers::speech::{AudioPart, SpeechError};

const DEFAULT_MODEL: &str = "qwen3-tts-flash";
/// 千问-TTS 系单请求文本上限（官方「其他模型 600 字符」，按输入字符计费）。
pub const MAX_TEXT_CHARS: usize = 600;
/// SSE 流式输出的音频规格（2026-08-15 联调实锤：首块自带 WAV 头即此规格；
/// 常量仅用于防御路径——vendor 不带头时按此封装）。
pub const PCM_SAMPLE_RATE: u32 = 24_000;
pub const PCM_BITS_PER_SAMPLE: u16 = 16;
pub const PCM_CHANNELS: u16 = 1;
/// 开启 SSE 流式输出的开关头。
const SSE_HEADER: (&str, &str) = ("X-DashScope-SSE", "enable");

/// 多模态生成端点路径（Qwen-TTS / Qwen-Audio 等共用 host）。
const GENERATION_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

/// Qwen-TTS 客户端（同步整段 + SSE 流式）。
#[derive(Debug, Clone)]
pub struct QwenTts {
  credentials: Arc<DashScopeCredentials>,
  region: DashScopeRegion,
  model: String,
  /// API host 根覆盖（默认按 region；测试指向 mock server）。
  base_url_override: Option<String>,
  http: reqwest::Client,
}

impl QwenTts {
  pub fn new(credentials: DashScopeCredentials) -> Result<Self, SpeechError> {
    let http = reqwest::Client::builder()
      .timeout(Duration::from_secs(120))
      .build()
      .map_err(|e| SpeechError::RequestBuild(format!("build reqwest client: {e}")))?;
    Ok(Self {
      credentials: Arc::new(credentials),
      region: DashScopeRegion::default(),
      model: DEFAULT_MODEL.into(),
      base_url_override: None,
      http,
    })
  }

  pub fn with_region(mut self, region: DashScopeRegion) -> Self {
    self.region = region;
    self
  }

  pub fn with_model(mut self, model: impl Into<String>) -> Self {
    self.model = model.into();
    self
  }

  /// 覆盖 API host 根（测试注入 mock server；None 恢复按 region）。
  pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
    self.base_url_override = base_url;
    self
  }

  fn endpoint(&self) -> String {
    let host = self.base_url_override.clone().unwrap_or_else(|| match self.region {
      DashScopeRegion::Beijing => "https://dashscope.aliyuncs.com".to_string(),
      DashScopeRegion::Singapore => "https://dashscope-intl.aliyuncs.com".to_string(),
    });
    format!("{host}{GENERATION_PATH}")
  }

  /// 整段合成：返回完整 WAV（24kHz/16bit/mono）。
  ///
  /// 超过 [`MAX_TEXT_CHARS`] 自动分段（按句切），各段直拼——流式首块自带
  /// 44 字节 WAV 头（2026-08-15 真实联调实锤，vendor 侧 RIFF/data size 用
  /// 0x7FFFFFFF 流式占位），次块起为裸 PCM，跨段直拼后由本方法回写准确的
  /// RIFF/data size（分段不改变计费，按输入字符累计）。
  pub async fn synthesize(&self, req: QwenTtsRequest<'_>) -> Result<Bytes, SpeechError> {
    let segments = split_text_segments(req.text, MAX_TEXT_CHARS);
    let mut audio: Vec<u8> = Vec::new();
    for segment in &segments {
      let segment_req = QwenTtsRequest { text: segment, ..req };
      let mut stream = self.synthesize_stream_raw(segment_req).await?;
      while let Some(part) = stream.next().await {
        audio.extend_from_slice(&part?.bytes);
      }
    }
    if audio.is_empty() {
      return Err(SpeechError::protocol("qwen-tts returned empty audio"));
    }
    if audio.len() >= 44 && &audio[0..4] == b"RIFF" {
      // 首块自带 WAV 头：回写准确长度（vendor 占位 0x7FFFFFFF，多数播放器容忍
      // 但不合规；本地已知总长，写准确值）
      let data_len = (audio.len() - 44) as u32;
      audio[4..8].copy_from_slice(&36u32.saturating_add(data_len).to_le_bytes());
      audio[40..44].copy_from_slice(&data_len.to_le_bytes());
      return Ok(Bytes::from(audio));
    }
    // 防御路径：vendor 变更不带头的裸 PCM 流（未见此形态，按声明规格封装）
    let mut wav = crate::providers::speech::pcm_wav_header(
      PCM_SAMPLE_RATE,
      PCM_CHANNELS,
      PCM_BITS_PER_SAMPLE,
      Some(audio.len()),
    );
    wav.extend_from_slice(&audio);
    Ok(Bytes::from(wav))
  }

  /// 流式合成：逐块返回音频——**首块自带 WAV 头**（调用方直拼 chunks 即得完整
  /// WAV，无需自行封装；`is_last` 块 bytes 为空）。超长文本自动分段，段间惰性
  /// 串行（前段耗尽才发起下一段，保持流式首块延迟；注意多段时仅第一段首块
  /// 带 WAV 头，段间直拼即合法 WAV 流）。
  pub async fn synthesize_stream(
    &self,
    req: QwenTtsRequest<'_>,
  ) -> Result<futures::stream::BoxStream<'static, Result<AudioPart, SpeechError>>, SpeechError> {
    let segments: Vec<String> =
      split_text_segments(req.text, MAX_TEXT_CHARS).into_iter().map(String::from).collect();
    if segments.len() == 1 {
      return self
        .synthesize_stream_raw(QwenTtsRequest { text: &segments[0], ..req })
        .await;
    }
    let client = self.clone();
    let model = req.model.map(String::from);
    let voice = req.voice.to_string();
    let language_type = req.language_type.map(String::from);
    let stream = async_stream::try_stream! {
      let total = segments.len();
      for (idx, segment) in segments.iter().enumerate() {
        let is_final_segment = idx + 1 == total;
        let mut stream = client
          .synthesize_stream_raw(QwenTtsRequest {
            text: segment,
            model: model.as_deref(),
            voice: &voice,
            language_type: language_type.as_deref(),
          })
          .await?;
        while let Some(part) = stream.next().await {
          let part = part?;
          yield AudioPart { bytes: part.bytes, is_last: part.is_last && is_final_segment };
        }
      }
    };
    Ok(Box::pin(stream))
  }

  /// 单段（≤ MAX_TEXT_CHARS）SSE 流式请求。
  async fn synthesize_stream_raw(
    &self,
    req: QwenTtsRequest<'_>,
  ) -> Result<futures::stream::BoxStream<'static, Result<AudioPart, SpeechError>>, SpeechError> {
    if req.text.is_empty() {
      return Err(SpeechError::request_build("qwen-tts text is empty"));
    }
    let body = QwenTtsSseRequest {
      model: req.model.unwrap_or(&self.model),
      input: QwenTtsInput {
        text: req.text,
        voice: req.voice,
        language_type: req.language_type.unwrap_or("Chinese"),
      },
      parameters: QwenTtsParameters { stream: true },
    };
    let response = self
      .http
      .post(self.endpoint())
      .header(AUTHORIZATION, format!("Bearer {}", self.credentials.api_key))
      .header(SSE_HEADER.0, SSE_HEADER.1)
      .header(CONTENT_TYPE, "application/json")
      .json(&body)
      .send()
      .await
      .map_err(SpeechError::from)?;
    let status = response.status();
    if !status.is_success() {
      let body = response.text().await.unwrap_or_default();
      return Err(SpeechError::Http { status: status.as_u16(), message: body });
    }

    let byte_stream = response.bytes_stream();
    let stream = async_stream::try_stream! {
      let mut byte_stream = std::pin::pin!(byte_stream);
      let mut parser = crate::providers::speech::SseDataParser::new();
      let mut seen_data = false;
      while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| SpeechError::StreamMidway(e.to_string()))?;
        for payload in parser.push_chunk(&chunk) {
          match parse_sse_chunk(&payload) {
            SseChunk::Data(pcm) => {
              seen_data = true;
              yield AudioPart { bytes: pcm, is_last: false };
            }
            SseChunk::Final => {
              // 末块 url 形态：音频已全部在 data 块中，直接收尾
              yield AudioPart { bytes: Bytes::new(), is_last: true };
              return;
            }
            SseChunk::Error { code, message } => {
              Err(classify_dashscope_code(&code, &message))?
            }
            SseChunk::Ignore => {}
          }
        }
      }
      if !seen_data {
        Err(SpeechError::protocol("qwen-tts stream ended without audio data"))?;
      }
    };
    Ok(stream.boxed())
  }
}

/// 合成请求（`text` 生命周期绑定调用方；`model` 缺省用客户端 `with_model` 值）。
#[derive(Debug, Clone, Copy)]
pub struct QwenTtsRequest<'a> {
  pub text: &'a str,
  /// 系统音色（"Cherry" 等）或 enrollment 返回的克隆音色名。
  pub voice: &'a str,
  /// 覆盖客户端默认 model（如克隆音色必须与其 target_model 一致）。
  pub model: Option<&'a str>,
  /// 缺省 "Chinese"；传 `Some("Auto")` 让模型自动判别。
  pub language_type: Option<&'a str>,
}

impl<'a> QwenTtsRequest<'a> {
  pub fn new(text: &'a str, voice: &'a str) -> Self {
    Self { text, voice, model: None, language_type: None }
  }

  /// 覆盖客户端默认 model（如克隆音色必须与其 target_model 一致）。
  pub fn with_model(mut self, model: &'a str) -> Self {
    self.model = Some(model);
    self
  }

  /// 覆盖默认 language_type（"Chinese"）。
  pub fn with_language_type(mut self, language_type: &'a str) -> Self {
    self.language_type = Some(language_type);
    self
  }
}

// =========================================================================
// SSE chunk 解析与错误分类
// =========================================================================

#[derive(Debug)]
enum SseChunk {
  /// `output.audio.data` 非空：base64 PCM 片段。
  Data(Bytes),
  /// `data` 空且 `url` 非空：结束块。
  Final,
  /// body 带 code/message 的业务错误（HTTP 200 内）。
  Error { code: String, message: String },
  /// usage / request_id 等本实现不消费的事件。
  Ignore,
}

fn parse_sse_chunk(payload: &str) -> SseChunk {
  let Ok(parsed) = serde_json::from_str::<serde_json::Value>(payload) else {
    return SseChunk::Ignore;
  };
  if let Some(code) = parsed.get("code").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) {
    let message =
      parsed.get("message").and_then(|m| m.as_str()).unwrap_or_default().to_string();
    return SseChunk::Error { code: code.to_string(), message };
  }
  let audio = match parsed.pointer("/output/audio") {
    Some(a) => a,
    None => return SseChunk::Ignore,
  };
  let data = audio.get("data").and_then(|d| d.as_str()).unwrap_or("");
  if !data.is_empty() {
    match base64::engine::general_purpose::STANDARD.decode(data) {
      Ok(bytes) => return SseChunk::Data(Bytes::from(bytes)),
      Err(_) => return SseChunk::Ignore,
    }
  }
  let url = audio.get("url").and_then(|u| u.as_str()).unwrap_or("");
  if !url.is_empty() {
    return SseChunk::Final;
  }
  SseChunk::Ignore
}

/// DashScope 错误码串分类（`code` 形如 `InvalidApiKey` / `Throttling.RequestRateQuota`
/// / `InternalError`，见官方错误码文档）。未知码按永久错误处理（保守：不盲重试）。
fn classify_dashscope_code(code: &str, message: &str) -> SpeechError {
  SpeechError::Vendor {
    code: code.to_string(),
    message: message.to_string(),
    rate_limited: code.starts_with("Throttling"),
    transient: code.starts_with("InternalError")
      || code.starts_with("ServiceUnavailable")
      || code.starts_with("Timeout"),
  }
}

/// 按句切分文本为 ≤ `max_chars` 字符的段（句末标点/换行优先，超长单句按逗号
/// 再硬切；分段不改变计费语义，输入字符累计）。
///
/// 切分基于 char 计数（官方上限按字符），返回的段切在字节边界上（char_indices
/// 映射，避免 char 计数当字节偏移错切 UTF-8）。
fn split_text_segments(text: &str, max_chars: usize) -> Vec<&str> {
  let indexed: Vec<(usize, char)> = text.char_indices().collect();
  if indexed.len() <= max_chars {
    return vec![text];
  }
  let sentence_end = |c: char| matches!(c, '。' | '！' | '？' | '；' | '!' | '?' | ';' | '\n');
  let mut segments = Vec::new();
  let mut start_char = 0usize;
  while start_char < indexed.len() {
    let remaining = indexed.len() - start_char;
    if remaining <= max_chars {
      segments.push(&text[indexed[start_char].0..]);
      break;
    }
    // 在窗口内找最后一个句末标点；没有则退逗号/顿号，再没有硬切
    let window = &indexed[start_char..start_char + max_chars];
    let cut = window
      .iter()
      .rposition(|&(_, c)| sentence_end(c))
      .unwrap_or_else(|| {
        window.iter().rposition(|&(_, c)| matches!(c, '，' | '、' | ',')).unwrap_or(max_chars - 1)
      });
    let end_char = start_char + cut + 1;
    let start_byte = indexed[start_char].0;
    let end_byte = indexed.get(end_char).map_or(text.len(), |(b, _)| *b);
    segments.push(&text[start_byte..end_byte]);
    start_char = end_char;
  }
  segments.retain(|s| !s.trim().is_empty());
  if segments.is_empty() { vec![text] } else { segments }
}

// =========================================================================
// 协议
// =========================================================================

#[derive(Debug, Serialize)]
struct QwenTtsSseRequest<'a> {
  model: &'a str,
  input: QwenTtsInput<'a>,
  parameters: QwenTtsParameters,
}

#[derive(Debug, Serialize)]
struct QwenTtsInput<'a> {
  text: &'a str,
  voice: &'a str,
  language_type: &'a str,
}

#[derive(Debug, Serialize)]
struct QwenTtsParameters {
  stream: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn split_short_text_single_segment() {
    assert_eq!(split_text_segments("你好", 600), vec!["你好"]);
    assert_eq!(split_text_segments("", 600), vec![""]);
  }

  #[test]
  fn split_long_text_at_sentence_boundaries() {
    let sentence = "这是一句话。"; // 6 chars
    let text = sentence.repeat(150); // 900 chars > 600
    let segments = split_text_segments(&text, 600);
    assert!(segments.len() >= 2);
    for s in &segments {
      assert!(s.chars().count() <= 600, "segment too long: {}", s.chars().count());
    }
    // 拼接无损
    assert_eq!(segments.concat(), text);
    // 每段以句末标点收尾（除最后一段）
    for s in &segments[..segments.len() - 1] {
      assert!(s.ends_with('。'), "segment not sentence-aligned: {s}");
    }
  }

  #[test]
  fn split_unpunctuated_text_hard_cuts() {
    let text = "字".repeat(1200);
    let segments = split_text_segments(&text, 600);
    assert!(segments.len() >= 2);
    assert_eq!(segments.concat(), text);
    for s in &segments {
      assert!(s.chars().count() <= 600);
    }
  }

  #[test]
  fn parse_sse_chunk_variants() {
    let pcm = base64::engine::general_purpose::STANDARD.encode([0x01, 0x02]);
    match parse_sse_chunk(&format!(r#"{{"output":{{"audio":{{"data":"{pcm}","url":""}}}}}}"#)) {
      SseChunk::Data(b) => assert_eq!(&b[..], &[0x01, 0x02]),
      other => panic!("expected Data, got {other:?}"),
    }
    match parse_sse_chunk(
      r#"{"output":{"audio":{"data":"","url":"http://oss/xxx.wav"}},"usage":{"characters":10}}"#,
    ) {
      SseChunk::Final => {}
      other => panic!("expected Final, got {other:?}"),
    }
    match parse_sse_chunk(r#"{"code":"InvalidApiKey","message":"bad key","request_id":"x"}"#) {
      SseChunk::Error { code, message } => {
        assert_eq!(code, "InvalidApiKey");
        assert_eq!(message, "bad key");
      }
      other => panic!("expected Error, got {other:?}"),
    }
    assert!(matches!(parse_sse_chunk(r#"{"usage":{"characters":10}}"#), SseChunk::Ignore));
  }

  #[test]
  fn classify_dashscope_codes() {
    assert!(classify_dashscope_code("Throttling.RequestRateQuota", "slow down").is_rate_limited());
    assert!(classify_dashscope_code("InternalError", "boom").is_retryable());
    assert!(!classify_dashscope_code("InvalidApiKey", "bad").is_retryable());
    assert!(!classify_dashscope_code("SomeUnknownCode", "?").is_retryable(), "未知码保守不重试");
  }

  #[test]
  fn sse_request_body_shape() {
    let body = QwenTtsSseRequest {
      model: "qwen3-tts-vc-2026-01-22",
      input: QwenTtsInput { text: "你好", voice: "Cherry", language_type: "Chinese" },
      parameters: QwenTtsParameters { stream: true },
    };
    let s = serde_json::to_value(&body).unwrap();
    assert_eq!(s["model"], "qwen3-tts-vc-2026-01-22");
    assert_eq!(s["input"]["voice"], "Cherry");
    assert_eq!(s["parameters"]["stream"], true);
  }

  fn sse_response(chunks: &[Vec<u8>]) -> String {
    let mut body = String::from(":HTTP_STATUS/200\n\n");
    for c in chunks {
      let data = base64::engine::general_purpose::STANDARD.encode(c);
      body.push_str(&format!(
        "id:1\nevent:result\ndata: {{\"output\":{{\"audio\":{{\"data\":\"{data}\",\"url\":\"\"}}}}}}\n\n"
      ));
    }
    body.push_str(
      "data: {\"output\":{\"audio\":{\"data\":\"\",\"url\":\"http://oss/full.wav\"}}}\n\n",
    );
    body
  }

  fn creds() -> DashScopeCredentials {
    DashScopeCredentials { api_key: "sk-test".into(), workspace_id: None }
  }

  /// 真实形态（2026-08-15 联调实锤）：首块自带 44 字节 WAV 头（size 为
  /// 0x7FFFFFFF 占位）+ 次块裸 PCM → 直拼 + 回写准确 size。
  #[tokio::test]
  async fn synthesize_stitches_wav_head_first_chunk_and_patches_size() {
    let server = wiremock::MockServer::start().await;
    // 首块：头 + data size 0x7FFFFFFF 占位 + 少量 PCM
    let mut first = crate::providers::speech::pcm_wav_header(24_000, 1, 16, None);
    assert_eq!(&first[4..8], &0xFFFF_FFFFu32.to_le_bytes(), "占位头前提");
    first.extend_from_slice(&[0xAA; 100]);
    let second = vec![0xBB; 50];
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path(GENERATION_PATH))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .insert_header("content-type", "text/event-stream")
          .set_body_string(sse_response(&[first.clone(), second.clone()])),
      )
      .mount(&server)
      .await;

    let client =
      QwenTts::new(creds()).unwrap().with_base_url(Some(server.uri())).with_model("qwen3-tts-flash");
    let wav = client.synthesize(QwenTtsRequest::new("你好", "Cherry")).await.unwrap();
    assert_eq!(wav.len(), 44 + 150);
    assert_eq!(&wav[0..4], b"RIFF");
    let riff = u32::from_le_bytes(wav[4..8].try_into().unwrap());
    let data = u32::from_le_bytes(wav[40..44].try_into().unwrap());
    assert_eq!(riff, 36 + 150, "RIFF size 回写准确值");
    assert_eq!(data, 150, "data size 回写准确值");
    assert_eq!(&wav[44..144], &[0xAA; 100]);
    assert_eq!(&wav[144..], &[0xBB; 50]);
  }

  /// 防御路径：vendor 返回不带头的裸 PCM（未见此形态）→ 按声明规格封装 WAV。
  #[tokio::test]
  async fn synthesize_wraps_bare_pcm_defensively() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .insert_header("content-type", "text/event-stream")
          .set_body_string(sse_response(&[vec![0x01, 0x02, 0x03, 0x04]])),
      )
      .mount(&server)
      .await;
    let client = QwenTts::new(creds()).unwrap().with_base_url(Some(server.uri()));
    let wav = client.synthesize(QwenTtsRequest::new("你好", "Cherry")).await.unwrap();
    assert_eq!(wav.len(), 48);
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
  }

  /// 多段（>600 字符）拼接：仅第一段首块带 WAV 头，段间直拼仍合法。
  #[tokio::test]
  async fn synthesize_multi_segment_stitches_with_single_header() {
    let server = wiremock::MockServer::start().await;
    let text = "字".repeat(1200); // 2 段
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({ "input": { "text": "字".repeat(600) } })))
      .respond_with(
        wiremock::ResponseTemplate::new(200).set_body_string(sse_response(&[{
          let mut c = crate::providers::speech::pcm_wav_header(24_000, 1, 16, None);
          c.extend_from_slice(&[0x0A; 10]);
          c
        }])),
      )
      .up_to_n_times(1)
      .mount(&server)
      .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({ "input": { "text": "字".repeat(600) } })))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .set_body_string(sse_response(&[vec![0x0B; 10]])),
      )
      .mount(&server)
      .await;

    let client = QwenTts::new(creds()).unwrap().with_base_url(Some(server.uri()));
    let wav = client.synthesize(QwenTtsRequest::new(&text, "Cherry")).await.unwrap();
    assert_eq!(wav.len(), 44 + 20);
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 20);
    assert_eq!(&wav[44..54], &[0x0A; 10]);
    assert_eq!(&wav[54..], &[0x0B; 10]);
  }
}
