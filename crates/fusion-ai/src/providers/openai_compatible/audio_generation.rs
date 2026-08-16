//! OpenAI Audio Generation API（TTS，类型本地化）。

use crate::providers::openai_compatible::Client;
use crate::providers::openai_compatible::errors::OpenAiCompatError;
use serde_json::json;

pub const TTS_1: &str = "tts-1";
pub const TTS_1_HD: &str = "tts-1-hd";

/// 语音合成请求。
#[derive(Clone, Debug)]
pub struct AudioGenerationRequest {
  pub text: String,
  pub voice: String,
  pub speed: f32,
  pub additional_params: Option<serde_json::Value>,
}

impl AudioGenerationRequest {
  pub fn new(text: impl Into<String>, voice: impl Into<String>) -> Self {
    Self { text: text.into(), voice: voice.into(), speed: 1.0, additional_params: None }
  }

  pub fn with_speed(mut self, speed: f32) -> Self {
    self.speed = speed;
    self
  }
}

/// 语音合成终态：音频字节。
#[derive(Debug, Clone)]
pub struct AudioGenerationResponse {
  pub audio: Vec<u8>,
}

#[derive(Clone)]
pub struct AudioGenerationModel {
  client: Client,
  pub model: String,
}

impl AudioGenerationModel {
  pub(crate) fn new(client: Client, model: &str) -> Self {
    Self { client, model: model.to_string() }
  }

  /// 文本转语音（响应体即音频字节）。
  pub async fn audio_generation(
    &self,
    request: AudioGenerationRequest,
  ) -> Result<AudioGenerationResponse, OpenAiCompatError> {
    let body = serde_json::to_vec(&json!({
        "model": self.model,
        "input": request.text,
        "voice": request.voice,
        "speed": request.speed,
    }))
    .map_err(OpenAiCompatError::from)?;

    let response = self.client.post_json("/audio/speech", body).send().await?;
    if !response.status().is_success() {
      return Err(Client::error_from_response(response).await);
    }

    let bytes = response.bytes().await.map_err(|e| OpenAiCompatError::Transport(e.to_string()))?;
    Ok(AudioGenerationResponse { audio: bytes.to_vec() })
  }
}
