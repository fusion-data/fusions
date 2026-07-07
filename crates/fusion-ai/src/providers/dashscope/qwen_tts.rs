//! 阿里云 DashScope **Qwen-TTS** 同步语音合成。
//!
//! 协议:`POST .../aigc/multimodal-generation/generation`,
//! `model=qwen3-tts-flash`,`input.voice="Cherry"` 等,
//! 返回 `output.audio.url`(24h 有效)或 `output.audio.data`(base64)。
//!
//! 参考:<https://help.aliyun.com/zh/model-studio/qwen-tts-api>

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use crate::providers::dashscope::{DashScopeCredentials, DashScopeRegion};

const DEFAULT_MODEL: &str = "qwen3-tts-flash";

/// Qwen-TTS 同步合成 client。
///
/// 设计上只覆盖 spike 评测样本生成场景:输入文本 + voice ID,返回 wav bytes。
#[derive(Debug, Clone)]
pub struct QwenTts {
  credentials: Arc<DashScopeCredentials>,
  region: DashScopeRegion,
  model: String,
  http: reqwest::Client,
}

impl QwenTts {
  pub fn new(credentials: DashScopeCredentials) -> Result<Self, QwenTtsError> {
    let http = reqwest::Client::builder()
      .timeout(Duration::from_secs(60))
      .build()
      .map_err(|e| QwenTtsError::Config(format!("build reqwest client: {e}")))?;
    Ok(Self {
      credentials: Arc::new(credentials),
      region: DashScopeRegion::default(),
      model: DEFAULT_MODEL.into(),
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

  /// 同步合成单条文本。返回的 `Bytes` 是 provider 原生输出(通常 wav / mp3)。
  ///
  /// `voice` 例:`"Cherry"` / `"Serena"` / `"Ethan"` / `"Chelsie"`。
  pub async fn synthesize(&self, text: &str, voice: &str) -> Result<QwenTtsAudio, QwenTtsError> {
    let endpoint = self.region.multimodal_endpoint();
    let body = QwenTtsRequest {
      model: &self.model,
      input: QwenTtsInput { text, voice, language_type: "Chinese" },
      parameters: QwenTtsParameters { stream: false },
    };
    let response = self
      .http
      .post(endpoint)
      .header(AUTHORIZATION, format!("Bearer {}", self.credentials.api_key))
      .header(CONTENT_TYPE, "application/json")
      .json(&body)
      .send()
      .await
      .map_err(|e| QwenTtsError::Network(e.to_string()))?;

    let status = response.status();
    let raw = response.text().await.map_err(|e| QwenTtsError::Network(e.to_string()))?;
    if !status.is_success() {
      return Err(QwenTtsError::Provider { status: status.as_u16(), body: raw });
    }

    let parsed: QwenTtsResponse =
      serde_json::from_str(&raw).map_err(|e| QwenTtsError::Protocol(format!("parse response: {e}; body={raw}")))?;
    let audio_field = parsed
      .output
      .audio
      .ok_or_else(|| QwenTtsError::Protocol(format!("response missing output.audio; body={raw}")))?;

    // 解码音频:优先 base64 data(非空),其次下载 url。
    // Qwen-TTS 非流式时通常 data 为空串、url 才是 OSS 下载地址。
    let bytes = if let Some(data_b64) = audio_field.data.as_ref().filter(|s| !s.is_empty()) {
      Bytes::from(
        base64::engine::general_purpose::STANDARD
          .decode(data_b64.as_bytes())
          .map_err(|e| QwenTtsError::Protocol(format!("base64 decode: {e}")))?,
      )
    } else if let Some(url) = audio_field.url.as_ref().filter(|s| !s.is_empty()) {
      let resp = self.http.get(url).send().await.map_err(|e| QwenTtsError::Network(e.to_string()))?;
      let status = resp.status();
      if !status.is_success() {
        return Err(QwenTtsError::Provider {
          status: status.as_u16(),
          body: format!("audio url GET failed: {status}"),
        });
      }
      resp.bytes().await.map_err(|e| QwenTtsError::Network(e.to_string()))?
    } else {
      return Err(QwenTtsError::Protocol(format!("response missing both audio.data and audio.url; body={raw}")));
    };

    Ok(QwenTtsAudio {
      bytes,
      sample_rate: audio_field.sample_rate.unwrap_or(24_000),
      mime: audio_field.format.unwrap_or_else(|| "wav".into()),
      request_id: parsed.request_id,
    })
  }
}

#[derive(Debug, Clone)]
pub struct QwenTtsAudio {
  pub bytes: Bytes,
  pub sample_rate: u32,
  /// `wav` / `mp3` / `pcm`。
  pub mime: String,
  pub request_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum QwenTtsError {
  #[error("config: {0}")]
  Config(String),
  #[error("network: {0}")]
  Network(String),
  #[error("protocol: {0}")]
  Protocol(String),
  #[error("provider returned {status}: {body}")]
  Provider { status: u16, body: String },
}

// =========================================================================
// 协议
// =========================================================================

#[derive(Debug, Serialize)]
struct QwenTtsRequest<'a> {
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

#[derive(Debug, Deserialize)]
struct QwenTtsResponse {
  #[serde(default)]
  request_id: Option<String>,
  output: QwenTtsOutput,
}

#[derive(Debug, Deserialize)]
struct QwenTtsOutput {
  #[serde(default)]
  audio: Option<QwenTtsAudioField>,
}

#[derive(Debug, Deserialize)]
struct QwenTtsAudioField {
  #[serde(default)]
  url: Option<String>,
  #[serde(default)]
  data: Option<String>,
  #[serde(default)]
  sample_rate: Option<u32>,
  #[serde(default)]
  format: Option<String>,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn serialize_request_body_shape() {
    let body = QwenTtsRequest {
      model: "qwen3-tts-flash",
      input: QwenTtsInput { text: "你好", voice: "Cherry", language_type: "Chinese" },
      parameters: QwenTtsParameters { stream: false },
    };
    let s = serde_json::to_value(&body).unwrap();
    assert_eq!(s["model"], "qwen3-tts-flash");
    assert_eq!(s["input"]["text"], "你好");
    assert_eq!(s["input"]["voice"], "Cherry");
    assert_eq!(s["parameters"]["stream"], false);
  }
}
