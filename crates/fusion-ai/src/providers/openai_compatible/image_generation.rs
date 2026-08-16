//! OpenAI Image Generation API（类型本地化）。

use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::providers::openai_compatible::client::{ApiResponse, Client};
use crate::providers::openai_compatible::errors::OpenAiCompatError;

pub const DALL_E_2: &str = "dall-e-2";
pub const DALL_E_3: &str = "dall-e-3";
pub const GPT_IMAGE_1: &str = "gpt-image-1";

/// 图像生成终态：解码后的图像字节 + 原始 wire 响应。
#[derive(Debug, Clone)]
pub struct ImageGenerationResponse {
  pub image: Vec<u8>,
  pub raw: ImageGenerationRawResponse,
}

/// 图像生成 wire 响应（OpenAI 方言）。
#[derive(Debug, Clone, Deserialize)]
pub struct ImageGenerationRawResponse {
  pub created: i64,
  pub data: Vec<ImageGenerationData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageGenerationData {
  #[serde(default)]
  pub b64_json: String,
  #[serde(default)]
  pub url: String,
}

/// 图像生成请求。
#[derive(Clone, Debug)]
pub struct ImageGenerationRequest {
  pub prompt: String,
  pub width: u32,
  pub height: u32,
  pub additional_params: Option<serde_json::Value>,
}

impl ImageGenerationRequest {
  pub fn new(prompt: impl Into<String>) -> Self {
    Self { prompt: prompt.into(), width: 1024, height: 1024, additional_params: None }
  }

  pub fn with_size(mut self, width: u32, height: u32) -> Self {
    self.width = width;
    self.height = height;
    self
  }
}

#[derive(Clone)]
pub struct ImageGenerationModel {
  client: Client,
  /// Name of the model (e.g.: dall-e-3)
  pub model: String,
}

impl ImageGenerationModel {
  pub(crate) fn new(client: Client, model: &str) -> Self {
    Self { client, model: model.to_string() }
  }

  /// 文生图。dall-e 系（非 gpt-image-1）强制 `response_format: b64_json` 回传。
  pub async fn image_generation(
    &self,
    generation_request: ImageGenerationRequest,
  ) -> Result<ImageGenerationResponse, OpenAiCompatError> {
    let mut request = json!({
        "model": self.model,
        "prompt": generation_request.prompt,
        "size": format!("{}x{}", generation_request.width, generation_request.height),
    });

    if self.model != *"gpt-image-1" {
      crate::json_utils::merge_inplace(
        &mut request,
        json!({
            "response_format": "b64_json"
        }),
      );
    }

    let body = serde_json::to_vec(&request).map_err(OpenAiCompatError::from)?;

    let response = self.client.post_json("/images/generations", body).send().await?;
    if !response.status().is_success() {
      return Err(Client::error_from_response(response).await);
    }

    let text = response.text().await.map_err(|e| OpenAiCompatError::Transport(e.to_string()))?;
    match serde_json::from_str::<ApiResponse<ImageGenerationRawResponse>>(&text).map_err(OpenAiCompatError::from)? {
      ApiResponse::Ok(response) => response.try_into(),
      ApiResponse::Err(err) => Err(err.into()),
    }
  }
}

impl TryFrom<ImageGenerationRawResponse> for ImageGenerationResponse {
  type Error = OpenAiCompatError;

  fn try_from(value: ImageGenerationRawResponse) -> Result<Self, Self::Error> {
    let first = value.data.first().ok_or_else(|| OpenAiCompatError::ResponseParse("missing image data".into()))?;

    let image = if !first.b64_json.is_empty() {
      base64::prelude::BASE64_STANDARD
        .decode(&first.b64_json)
        .map_err(|err| OpenAiCompatError::ResponseParse(err.to_string()))?
    } else if !first.url.is_empty() {
      // URL 回传方言：同步下载（沿用 fork 基线的 ureq 路径）
      ureq::get(&first.url)
        .call()
        .map_err(|err| OpenAiCompatError::ResponseParse(err.to_string()))?
        .into_body()
        .read_to_vec()
        .map_err(|err| OpenAiCompatError::ResponseParse(err.to_string()))?
    } else {
      return Err(OpenAiCompatError::ResponseParse("image data has neither b64_json nor url".into()));
    };

    Ok(Self { image, raw: value })
  }
}
