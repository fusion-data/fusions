//! OpenAI Transcription API（类型本地化，reqwest multipart）。

use serde::Deserialize;

use crate::providers::openai_compatible::client::{ApiResponse, Client};
use crate::providers::openai_compatible::errors::OpenAiCompatError;

pub const WHISPER_1: &str = "whisper-1";

/// Transcription wire 响应（OpenAI 方言）。
#[derive(Debug, Deserialize)]
pub struct TranscriptionResponse {
  pub text: String,
}

/// Transcription 请求。
#[derive(Clone, Debug)]
pub struct TranscriptionRequest {
  /// 音频文件数据
  pub data: Vec<u8>,
  /// 文件名（multipart filename）
  pub filename: String,
  /// ISO-639-1 语言代码
  pub language: Option<String>,
  /// 引导词
  pub prompt: Option<String>,
  pub temperature: Option<f64>,
  /// extra 表单字段（provider 专属参数）
  pub additional_params: Option<serde_json::Value>,
}

impl TranscriptionRequest {
  pub fn new(data: Vec<u8>, filename: impl Into<String>) -> Self {
    Self { data, filename: filename.into(), language: None, prompt: None, temperature: None, additional_params: None }
  }

  pub fn with_language(mut self, language: impl Into<String>) -> Self {
    self.language = Some(language.into());
    self
  }

  pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
    self.prompt = Some(prompt.into());
    self
  }

  pub fn with_temperature(mut self, temperature: f64) -> Self {
    self.temperature = Some(temperature);
    self
  }
}

#[derive(Clone)]
pub struct TranscriptionModel {
  client: Client,
  /// Name of the model (e.g.: whisper-1)
  pub model: String,
}

impl TranscriptionModel {
  pub(crate) fn new(client: Client, model: &str) -> Self {
    Self { client, model: model.to_string() }
  }

  /// 语音转文字（multipart/form-data）。
  pub async fn transcription(&self, request: TranscriptionRequest) -> Result<TranscriptionResponse, OpenAiCompatError> {
    let mut form = reqwest::multipart::Form::new()
      .text("model", self.model.clone())
      .part("file", reqwest::multipart::Part::bytes(request.data).file_name(request.filename));

    if let Some(language) = request.language {
      form = form.text("language", language);
    }

    if let Some(prompt) = request.prompt {
      form = form.text("prompt", prompt);
    }

    if let Some(temperature) = request.temperature {
      form = form.text("temperature", temperature.to_string());
    }

    if let Some(additional_params) = request.additional_params {
      for (key, value) in additional_params.as_object().unwrap_or(&serde_json::Map::new()) {
        form = form.text(key.to_owned(), value.to_string());
      }
    }

    let response = self
      .client
      .http_client
      .post(self.client.endpoint("/audio/transcriptions"))
      .bearer_auth(&self.client.api_key)
      .multipart(form)
      .send()
      .await?;

    let status = response.status().as_u16();
    let body = response.bytes().await.map_err(|e| OpenAiCompatError::Transport(e.to_string()))?;

    if (200..300).contains(&status) {
      match serde_json::from_slice::<ApiResponse<TranscriptionResponse>>(&body).map_err(OpenAiCompatError::from)? {
        ApiResponse::Ok(response) => Ok(response),
        ApiResponse::Err(api_error_response) => Err(api_error_response.into()),
      }
    } else {
      let text = String::from_utf8_lossy(&body).to_string();
      Err(OpenAiCompatError::Http { status, message: text })
    }
  }
}
