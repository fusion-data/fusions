//! MiniMax **T2A V2** 语音合成 + 声音复刻（api.minimaxi.com）。
//!
//! 协议：
//! - 合成：`POST /v1/t2a_v2?GroupId=...`（`stream=true` 时 SSE，`data:` 行内
//!   `data.audio` 为 **hex 编码**音频块；末块携带 `extra_info`；`subtitle_enable`
//!   时 `subtitle_file` 为 hex SRT 内容或 OSS URL）
//! - 复刻：`POST /v1/files/upload`（multipart，purpose=voice_clone）→
//!   `POST /v1/voice_clone`（voice_id 由调用方自定义，MiniMax 不生成新 ID）
//!
//! 业务错误模式：HTTP 200 + `base_resp.status_code != 0`（1002 限流 /
//! 1008 余额不足 / 2013 参数错等）；鉴权 Bearer。
//!
//! 参考：platform.minimaxi.com/docs/api-reference

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use serde::Deserialize;

use crate::providers::speech::{AudioPart, SpeechError, SseDataParser, decode_hex};

/// MiniMax 默认 API 端点（中国平台；国际平台 api.minimax.io 不在支持范围）。
const DEFAULT_BASE_URL: &str = "https://api.minimaxi.com";
/// 默认合成模型（2026-08-16 切换：speech-2.8-turbo，2.0 元/万字符；老默认
/// speech-02-hd 3.5 元/万字符，hd/turbo 参数面同构——字幕直返/发音词典/情感
/// 实测可用，env `MINIMAX_TTS_MODEL` 覆盖）。
pub const DEFAULT_MODEL: &str = "speech-2.8-turbo";
/// T2A V2 业务错误码：请求频率超限（RPM/TPM）。返回此码应退避重试
/// （MiniMax 不返回 Retry-After，用固定窗口）。
const RATE_LIMIT_CODE: i32 = 1002;
/// 永久性业务码（余额不足 / 鉴权失败 / 参数错）：重试无意义。
const PERMANENT_CODES: [i32; 3] = [1008, 1004, 2013];
/// subtitle OSS 下载整体超时（连接 + 读全量）。SRT 为 KB 级小文件，30s 裕量充足；
/// 防 OSS 连接挂死阻塞整页合成。
const SUBTITLE_DOWNLOAD_TIMEOUT_SECS: u64 = 30;

/// MiniMax TTS 客户端（合成 + 复刻）。
///
/// 持 `api_key`，按 framework-conventions §2 手写脱敏 Debug（MUST NOT derive）。
pub struct MinimaxTts {
  api_key: Option<String>,
  group_id: Option<String>,
  base_url: String,
  /// 合成模型（客户端级默认，`T2aRequest.model` 由调用方注入本值）。
  model: String,
  http: reqwest::Client,
}

impl std::fmt::Debug for MinimaxTts {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MinimaxTts")
      .field("api_key", &self.api_key.as_ref().map(|_| "<REDACTED>"))
      .field("group_id", &self.group_id)
      .field("base_url", &self.base_url)
      .field("model", &self.model)
      .finish_non_exhaustive()
  }
}

impl MinimaxTts {
  pub fn new(api_key: Option<String>, group_id: Option<String>) -> Self {
    Self::with_base_url(api_key, group_id, DEFAULT_BASE_URL.to_string())
  }

  /// 从环境变量构造（`MINIMAX_API_KEY` + `MINIMAX_GROUP_ID`；缺任一返回
  /// `is_configured()=false` 实例，调用方决定语义——本客户端不做兜底猜测。
  /// 模型可经 `MINIMAX_TTS_MODEL` 覆盖，缺省 [`DEFAULT_MODEL`]）。
  pub fn from_env() -> Self {
    let key = std::env::var("MINIMAX_API_KEY").ok().filter(|s| !s.is_empty());
    let group = std::env::var("MINIMAX_GROUP_ID").ok().filter(|s| !s.is_empty());
    let base_url = std::env::var("MINIMAX_BASE_URL")
      .ok()
      .filter(|s| !s.is_empty())
      .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let model = std::env::var("MINIMAX_TTS_MODEL")
      .ok()
      .filter(|s| !s.is_empty())
      .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    Self::with_base_url(key, group, base_url).with_model(model)
  }

  /// 带 base_url 构造（测试指向 mock server）。
  pub fn with_base_url(api_key: Option<String>, group_id: Option<String>, base_url: String) -> Self {
    Self {
      api_key,
      group_id,
      base_url,
      model: DEFAULT_MODEL.to_string(),
      http: reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("reqwest client build"),
    }
  }

  /// 覆盖客户端默认合成模型。
  pub fn with_model(mut self, model: impl Into<String>) -> Self {
    self.model = model.into();
    self
  }

  /// 客户端默认合成模型（`T2aRequest.model` / `clone_voice` 的 model 参数取此值）。
  pub fn model(&self) -> &str {
    &self.model
  }

  pub fn is_configured(&self) -> bool {
    self.api_key.is_some() && self.group_id.is_some()
  }

  fn require_config(&self) -> Result<(&str, &str), SpeechError> {
    match (&self.api_key, &self.group_id) {
      (Some(k), Some(g)) => Ok((k.as_str(), g.as_str())),
      _ => Err(SpeechError::request_build(
        "minimax api_key or group_id missing (MINIMAX_API_KEY / MINIMAX_GROUP_ID)",
      )),
    }
  }

  /// base_resp 业务错误分类（T2A 非流式 / 流式 / 复刻共用）。
  fn classify_base_resp(status_code: i32, status_msg: &str) -> SpeechError {
    SpeechError::Vendor {
      code: status_code.to_string(),
      message: status_msg.to_string(),
      rate_limited: status_code == RATE_LIMIT_CODE,
      // 已知永久码之外的业务码按瞬态处理（服务端抖动），调用方可退避重试
      transient: !PERMANENT_CODES.contains(&status_code) && status_code != RATE_LIMIT_CODE,
    }
  }

  /// HTTP 错误分类（429 限流 / 5xx 瞬态 / 其余 4xx 永久）。
  fn classify_http(status: reqwest::StatusCode, body: String) -> SpeechError {
    SpeechError::Http { status: status.as_u16(), message: body }
  }

  // =====================================================================
  // 合成
  // =====================================================================

  /// 非流式整段合成（含可选直返字幕）。
  pub async fn synthesize(&self, req: T2aRequest<'_>) -> Result<T2aAudio, SpeechError> {
    let (key, group) = self.require_config()?;
    let body = t2a_body(&req, false);
    let response = self
      .http
      .post(format!("{}/v1/t2a_v2?GroupId={group}", self.base_url))
      .bearer_auth(key)
      .json(&body)
      .send()
      .await
      .map_err(SpeechError::from)?;
    let status = response.status();
    if !status.is_success() {
      let body = response.text().await.unwrap_or_default();
      return Err(Self::classify_http(status, body));
    }
    let parsed: T2aResponse = response
      .json()
      .await
      .map_err(|e| SpeechError::protocol(format!("parse t2a_v2 response: {e}")))?;
    parse_t2a_response(&parsed)
  }

  /// 流式合成（SSE `data:` 行 → hex 解码音频块；末块 `extra_info` 判 `is_last`）。
  pub async fn synthesize_stream(
    &self,
    req: T2aRequest<'_>,
  ) -> Result<futures::stream::BoxStream<'static, Result<AudioPart, SpeechError>>, SpeechError> {
    let (key, group) = self.require_config()?;
    let body = t2a_body(&req, true);
    let response = self
      .http
      .post(format!("{}/v1/t2a_v2?GroupId={group}", self.base_url))
      .bearer_auth(key)
      .json(&body)
      .send()
      .await
      .map_err(SpeechError::from)?;
    let status = response.status();
    if !status.is_success() {
      let body = response.text().await.unwrap_or_default();
      return Err(Self::classify_http(status, body));
    }

    let byte_stream = response.bytes_stream();
    let stream = async_stream::try_stream! {
      let mut byte_stream = std::pin::pin!(byte_stream);
      let mut parser = SseDataParser::new();
      while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| SpeechError::StreamMidway(e.to_string()))?;
        for payload in parser.push_chunk(&chunk) {
          let Ok(parsed) = serde_json::from_str::<T2aStreamChunk>(&payload) else {
            continue; // 跳过非 JSON 行（SSE 注释等）
          };
          if let Some(base) = &parsed.base_resp
            && base.status_code != 0
          {
            Err(Self::classify_base_resp(base.status_code, &base.status_msg))?;
          }
          let is_last = parsed.extra_info.is_some();
          let audio_hex = parsed.data.as_ref().map(|d| d.audio.as_str()).unwrap_or("");
          let audio = if audio_hex.is_empty() {
            Vec::new()
          } else {
            decode_hex(audio_hex).unwrap_or_default()
          };
          yield AudioPart { bytes: Bytes::from(audio), is_last };
          if is_last {
            return;
          }
        }
      }
    };
    Ok(stream.boxed())
  }

  // =====================================================================
  // 声音复刻（两步）
  // =====================================================================

  /// 声音复刻：multipart 上传样本 → voice_clone 确认。
  ///
  /// `voice_id` 由调用方自定义（MiniMax 不生成新 ID，克隆成功后直接用该 ID 调
  /// 合成）；无自带试听产物（调用方需自行合成预览）。
  pub async fn clone_voice(
    &self,
    sample_audio: &[u8],
    voice_id: &str,
    model: &str,
  ) -> Result<String, SpeechError> {
    let (key, group) = self.require_config()?;
    if sample_audio.is_empty() {
      return Err(SpeechError::request_build("clone_voice sample audio is empty"));
    }
    if voice_id.is_empty() {
      return Err(SpeechError::request_build("clone_voice voice_id is empty"));
    }

    // Step 1: 上传音频（multipart/form-data, purpose=voice_clone）
    let part = reqwest::multipart::Part::bytes(sample_audio.to_vec())
      .file_name("sample.mp3")
      .mime_str("audio/mpeg")
      .map_err(|e| SpeechError::request_build(format!("mime construct: {e}")))?;
    let form = reqwest::multipart::Form::new().text("purpose", "voice_clone").part("file", part);
    let upload_resp = self
      .http
      .post(format!("{}/v1/files/upload?GroupId={group}", self.base_url))
      .bearer_auth(key)
      .multipart(form)
      .send()
      .await
      .map_err(SpeechError::from)?;
    let status = upload_resp.status();
    if !status.is_success() {
      let body = upload_resp.text().await.unwrap_or_default();
      return Err(Self::classify_http(status, body));
    }
    let upload_parsed: FileUploadResponse = upload_resp
      .json()
      .await
      .map_err(|e| SpeechError::protocol(format!("parse files/upload response: {e}")))?;
    if let Some(base) = &upload_parsed.base_resp
      && base.status_code != 0
    {
      return Err(Self::classify_base_resp(base.status_code, &base.status_msg));
    }
    let file_id = upload_parsed
      .file
      .as_ref()
      .map(|f| f.file_id.clone())
      .ok_or_else(|| SpeechError::protocol("files/upload returned null file_id"))?;

    // Step 2: voice_clone（JSON body: voice_id + file_id + model）
    let body = serde_json::json!({ "voice_id": voice_id, "file_id": file_id, "model": model });
    let resp = self
      .http
      .post(format!("{}/v1/voice_clone?GroupId={group}", self.base_url))
      .bearer_auth(key)
      .json(&body)
      .send()
      .await
      .map_err(SpeechError::from)?;
    let status = resp.status();
    if !status.is_success() {
      let body = resp.text().await.unwrap_or_default();
      return Err(Self::classify_http(status, body));
    }
    let parsed: VoiceCloneResponse = resp
      .json()
      .await
      .map_err(|e| SpeechError::protocol(format!("parse voice_clone response: {e}")))?;
    if let Some(base) = &parsed.base_resp
      && base.status_code != 0
    {
      return Err(Self::classify_base_resp(base.status_code, &base.status_msg));
    }
    Ok(voice_id.to_string())
  }

  /// 下载 subtitle OSS URL 取回 SRT 内容。
  ///
  /// 用 `no_proxy` client 构造（规避 Aliyun OSS SSL 经代理握手失败的已知问题）。
  /// 下载失败语义由调用方决定（典型：降级 warn + 无字幕，不阻塞音频）。
  pub async fn download_subtitle(&self, url: &str) -> Result<Bytes, SpeechError> {
    let client = reqwest::Client::builder()
      .no_proxy()
      .timeout(Duration::from_secs(SUBTITLE_DOWNLOAD_TIMEOUT_SECS))
      .build()
      .map_err(|e| SpeechError::Transport(e.to_string()))?;
    let resp = client
      .get(url)
      .send()
      .await
      .map_err(SpeechError::from)?
      .error_for_status()
      .map_err(SpeechError::from)?;
    Ok(resp.bytes().await?)
  }
}

/// T2A V2 合成请求（最终协议参数形态——emotion 枚举串、发音词典条目等 vendor
/// 专属参数由调用方完成业务转译后传入）。
#[derive(Debug, Clone, Copy)]
pub struct T2aRequest<'a> {
  pub text: &'a str,
  pub voice_id: &'a str,
  /// 语速倍率（1.0 = 原速，MiniMax 标度 0.5-2.0）。
  pub speed: f32,
  /// 音量（0-10，默认 1.0）。
  pub vol: f32,
  /// 情感枚举串（None 省略字段让模型自动匹配）。
  pub emotion: Option<&'a str>,
  /// 音频格式（"mp3" / "pcm" / "wav"）。
  pub format: &'a str,
  pub sample_rate: u32,
  /// 要求直返字幕时间戳（subtitle_file）。
  pub subtitle_enable: bool,
  /// 发音词典 tone 条目（`word/(pin1)(yin2)` / `word/target` 形态，调用方构造）。
  pub pronunciation_tones: &'a [String],
  pub model: &'a str,
}

impl<'a> T2aRequest<'a> {
  pub fn new(text: &'a str, voice_id: &'a str) -> Self {
    Self {
      text,
      voice_id,
      speed: 1.0,
      vol: 1.0,
      emotion: None,
      format: "mp3",
      sample_rate: 32_000,
      subtitle_enable: false,
      pronunciation_tones: &[],
      model: DEFAULT_MODEL,
    }
  }
}

/// 非流式合成产物。
#[derive(Debug, Clone)]
pub struct T2aAudio {
  pub audio: Bytes,
  /// 直返字幕（hex 形态 subtitle_file 已解码；URL 形态见 `subtitle_url`）。
  pub subtitle: Option<Bytes>,
  /// OSS URL 形态的 subtitle_file（由调用方异步下载；下载失败可降级无字幕）。
  pub subtitle_url: Option<String>,
}

fn t2a_body(req: &T2aRequest<'_>, stream: bool) -> serde_json::Value {
  let mut voice_setting = serde_json::json!({
    "voice_id": req.voice_id,
    "speed": req.speed,
    "vol": req.vol,
  });
  if let Some(emotion) = req.emotion {
    voice_setting["emotion"] = serde_json::Value::String(emotion.to_string());
  }
  let mut body = serde_json::json!({
    "model": req.model,
    "text": req.text,
    "stream": stream,
    "voice_setting": voice_setting,
    "audio_setting": {
      "sample_rate": req.sample_rate,
      "format": req.format,
    },
  });
  if !req.pronunciation_tones.is_empty() {
    body["pronunciation_dict"] =
      serde_json::json!({ "tone": req.pronunciation_tones });
  }
  if req.subtitle_enable {
    body["subtitle_enable"] = serde_json::Value::Bool(true);
  }
  body
}

/// 非流式响应解析：base_resp 业务错检查 + hex 解码 audio/subtitle_file 分流。
fn parse_t2a_response(parsed: &T2aResponse) -> Result<T2aAudio, SpeechError> {
  if let Some(base) = &parsed.base_resp
    && base.status_code != 0
  {
    return Err(MinimaxTts::classify_base_resp(base.status_code, &base.status_msg));
  }
  let audio_hex = parsed.data.as_ref().map(|d| d.audio.as_str()).unwrap_or("");
  let audio = decode_hex(audio_hex)
    .ok_or_else(|| SpeechError::protocol("t2a_v2 audio hex decode failed: odd length"))?;
  if audio.is_empty() {
    return Err(SpeechError::protocol("t2a_v2 returned empty audio"));
  }
  let raw_subtitle = parsed
    .data
    .as_ref()
    .and_then(|d| d.subtitle_file.as_deref())
    .filter(|s| !s.is_empty());
  let (subtitle, subtitle_url) = match raw_subtitle {
    Some(s) if s.starts_with("http") => (None, Some(s.to_string())),
    Some(s) => (decode_hex(s).map(Bytes::from), None),
    None => (None, None),
  };
  Ok(T2aAudio { audio: Bytes::from(audio), subtitle, subtitle_url })
}

// =========================================================================
// 协议
// =========================================================================

/// /v1/files/upload 响应（voice_clone step 1）。
#[derive(Debug, Deserialize)]
struct FileUploadResponse {
  #[serde(default)]
  base_resp: Option<T2aBaseResp>,
  #[serde(default)]
  file: Option<FileUploadFile>,
}

#[derive(Debug, Deserialize)]
struct FileUploadFile {
  /// file_id 是数字（如 426692831949097），用 Value 兼容数字/字符串。
  file_id: serde_json::Value,
}

/// /v1/voice_clone 响应（step 2）。
#[derive(Debug, Deserialize)]
struct VoiceCloneResponse {
  #[serde(default)]
  base_resp: Option<T2aBaseResp>,
}

#[derive(Debug, Deserialize)]
struct T2aResponse {
  /// 业务错误时 HTTP 200 + status_code != 0。
  #[serde(default)]
  base_resp: Option<T2aBaseResp>,
  #[serde(default)]
  data: Option<T2aAudioData>,
}

#[derive(Debug, Deserialize)]
struct T2aBaseResp {
  status_code: i32,
  #[serde(default)]
  status_msg: String,
}

#[derive(Debug, Deserialize)]
struct T2aAudioData {
  /// hex 编码的音频字节。
  audio: String,
  /// subtitle_enable=true 时返回（hex SRT 内容或 OSS URL）。
  #[serde(default)]
  subtitle_file: Option<String>,
}

/// stream=true 的单个 SSE chunk。
#[derive(Debug, Deserialize)]
struct T2aStreamChunk {
  #[serde(default)]
  data: Option<T2aAudioData>,
  /// 最后一个 chunk 才有（is_last 判据）。
  #[serde(default)]
  extra_info: Option<serde_json::Value>,
  #[serde(default)]
  base_resp: Option<T2aBaseResp>,
}

#[cfg(test)]
mod tests {
  use super::*;

  fn client(server_url: String) -> MinimaxTts {
    MinimaxTts::with_base_url(Some("test-key".into()), Some("test-group".into()), server_url)
  }

  fn hex(bytes: &[u8]) -> String {
    crate::providers::speech::encode_hex(bytes)
  }

  #[tokio::test]
  async fn synthesize_sends_body_and_decodes_hex_audio() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path("/v1/t2a_v2"))
      .and(wiremock::matchers::query_param("GroupId", "test-group"))
      .and(wiremock::matchers::header("authorization", "Bearer test-key"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({
        "model": DEFAULT_MODEL,
        "text": "你好",
        "stream": false,
        "voice_setting": { "voice_id": "v1", "speed": 1.0, "vol": 1.0 },
        "audio_setting": { "sample_rate": 32000, "format": "mp3" },
        "subtitle_enable": true,
      })))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "base_resp": { "status_code": 0, "status_msg": "success" },
        "data": { "audio": hex(b"mp3-bytes"), "subtitle_file": hex("1\n00:00:00 --> 00:00:01\n你\n".as_bytes()) }
      })))
      .expect(1)
      .mount(&server)
      .await;

    let c = client(server.uri());
    let req = T2aRequest { text: "你好", voice_id: "v1", subtitle_enable: true, ..T2aRequest::new("你好", "v1") };
    let audio = c.synthesize(req).await.unwrap();
    assert_eq!(&audio.audio[..], b"mp3-bytes");
    assert_eq!(&audio.subtitle.unwrap()[..], "1\n00:00:00 --> 00:00:01\n你\n".as_bytes());
    assert!(audio.subtitle_url.is_none());
  }

  #[tokio::test]
  async fn synthesize_subtitle_url_form_fills_url_field() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path("/v1/t2a_v2"))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "base_resp": { "status_code": 0 },
        "data": { "audio": hex(b"aa"), "subtitle_file": "https://oss.example.com/x.srt" }
      })))
      .mount(&server)
      .await;
    let c = client(server.uri());
    let audio = c.synthesize(T2aRequest::new("t", "v")).await.unwrap();
    assert_eq!(audio.subtitle_url.as_deref(), Some("https://oss.example.com/x.srt"));
    assert!(audio.subtitle.is_none());
  }

  #[tokio::test]
  async fn synthesize_classifies_business_and_http_errors() {
    // 1002 → rate limited
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
        serde_json::json!({ "base_resp": { "status_code": 1002, "status_msg": "rate" } }),
      ))
      .mount(&server)
      .await;
    let err = client(server.uri()).synthesize(T2aRequest::new("t", "v")).await.unwrap_err();
    assert!(err.is_rate_limited(), "{err:?}");

    // 2013 → permanent
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
        serde_json::json!({ "base_resp": { "status_code": 2013, "status_msg": "param" } }),
      ))
      .mount(&server)
      .await;
    let err = client(server.uri()).synthesize(T2aRequest::new("t", "v")).await.unwrap_err();
    assert!(!err.is_retryable(), "{err:?}");

    // HTTP 429 → rate limited
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(429))
      .mount(&server)
      .await;
    let err = client(server.uri()).synthesize(T2aRequest::new("t", "v")).await.unwrap_err();
    assert!(err.is_rate_limited(), "{err:?}");
  }

  #[tokio::test]
  async fn synthesize_stream_yields_hex_chunks_with_extra_info_last() {
    let server = wiremock::MockServer::start().await;
    let body = format!(
      "data: {}\n\ndata: {}\n\n",
      serde_json::json!({ "data": { "audio": hex(b"aaa") } }),
      serde_json::json!({ "data": { "audio": hex(b"bbb") }, "extra_info": { "audio_length": 2 } }),
    );
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path("/v1/t2a_v2"))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .insert_header("content-type", "text/event-stream")
          .set_body_string(body),
      )
      .mount(&server)
      .await;
    let c = client(server.uri());
    let mut stream = c.synthesize_stream(T2aRequest::new("t", "v")).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(&first.bytes[..], b"aaa");
    assert!(!first.is_last);
    let last = stream.next().await.unwrap().unwrap();
    assert_eq!(&last.bytes[..], b"bbb");
    assert!(last.is_last);
    assert!(stream.next().await.is_none());
  }

  #[tokio::test]
  async fn clone_voice_two_steps_returns_custom_voice_id() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path("/v1/files/upload"))
      .respond_with(
        wiremock::ResponseTemplate::new(200)
          .set_body_json(serde_json::json!({ "file": { "file_id": 123456, "bytes": 1024 } })),
      )
      .expect(1)
      .mount(&server)
      .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path("/v1/voice_clone"))
      .and(wiremock::matchers::body_partial_json(
        serde_json::json!({ "voice_id": "my-voice", "file_id": 123456, "model": "m" }),
      ))
      .respond_with(
        wiremock::ResponseTemplate::new(200).set_body_json(
          serde_json::json!({ "base_resp": { "status_code": 0, "status_msg": "success" } }),
        ),
      )
      .expect(1)
      .mount(&server)
      .await;

    let voice = client(server.uri())
      .clone_voice(b"fake mp3", "my-voice", "m")
      .await
      .unwrap();
    assert_eq!(voice, "my-voice");
  }

  #[test]
  fn unconfigured_client_rejects_calls() {
    let c = MinimaxTts::new(None, Some("g".into()));
    assert!(!c.is_configured());
    assert!(matches!(c.require_config(), Err(SpeechError::RequestBuild(_))));
  }
}
