//! OpenAI Image Edit API（类型本地化，reqwest multipart）。

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::providers::openai_compatible::client::{ApiResponse, Client};
use crate::providers::openai_compatible::errors::OpenAiCompatError;

// Model constants are exported from image_generation module
pub use super::image_generation::{DALL_E_2, GPT_IMAGE_1};

/// Image edit response data (supports both URL and base64 formats)
#[derive(Debug, Clone, Deserialize)]
pub struct ImageEditData {
  /// URL to the generated image (optional, some providers return base64 instead)
  #[serde(default)]
  pub url: String,
  /// Base64 encoded image data
  #[serde(default)]
  pub b64_json: String,
}

/// Request for image editing
#[derive(Clone, Debug)]
pub struct ImageEditRequest {
  /// The images to edit. For DALL-E 2: exactly 1 PNG image.
  /// For gpt-image-1: 1-16 images in PNG/JPG/WEBP format.
  pub images: Vec<Vec<u8>>,
  /// Optional mask image. Must be a valid PNG file with same dimensions as the first image.
  /// The fully transparent areas (alpha = 0) indicate where the image should be edited.
  /// Only supported for DALL-E 2 when using a single image.
  pub mask_data: Option<Vec<u8>>,
  /// A text description of the desired image(s).
  pub prompt: String,
  /// The size of the generated images in pixels.
  /// For gpt-image-1: "1024x1024", "1536x1024", "1024x1536", or "auto"
  /// For DALL-E 2: "256x256", "512x512", or "1024x1024"
  pub size: String,
  /// The number of images to generate (1-10). Defaults to 1.
  pub n: Option<u64>,
  /// The unique identifier for the end-user.
  pub user: Option<String>,
  /// The quality of the image that will be generated (gpt-image-1 only).
  /// Options: "low", "medium", "high". Defaults to "auto".
  pub quality: Option<String>,
  /// Allows setting transparency for the background of the generated image(s) (gpt-image-1 only).
  /// Options: "transparent", "opaque", "auto".
  pub background: Option<String>,
  /// The format in which the generated images are returned (gpt-image-1 only).
  /// Options: "png", "jpeg", "webp". Defaults to "png".
  pub output_format: Option<String>,
  /// The compression level for the generated images, 0-100 (gpt-image-1 with webp/jpeg only).
  pub output_compression: Option<u64>,
  /// Control how much effort the model will exert to match the style (gpt-image-1 only).
  /// Options: "high", "low". Defaults to "low".
  pub input_fidelity: Option<String>,
  /// The number of partial images to generate for streaming (gpt-image-1 only).
  /// Value must be 0-3. When 0, returns a single image.
  pub partial_images: Option<u64>,
  /// Whether to edit the image in streaming mode (gpt-image-1 only).
  pub stream: Option<bool>,
}

impl ImageEditRequest {
  /// Create a new image edit request with images
  /// Accepts either a single image or multiple images for gpt-image-1
  pub fn new(images: Vec<Vec<u8>>, prompt: String, size: String) -> Self {
    Self {
      images,
      mask_data: None,
      prompt,
      size,
      n: Some(1),
      user: None,
      quality: None,
      background: None,
      output_format: None,
      output_compression: None,
      input_fidelity: None,
      partial_images: None,
      stream: None,
    }
  }

  /// Create a new image edit request with a single image (convenience method)
  pub fn new_single(image_data: Vec<u8>, prompt: String, size: String) -> Self {
    Self::new(vec![image_data], prompt, size)
  }

  /// Set the mask data (for DALL-E 2 single image editing)
  pub fn with_mask(mut self, mask_data: Vec<u8>) -> Self {
    self.mask_data = Some(mask_data);
    self
  }

  /// Set the number of images to generate
  pub fn with_n(mut self, n: u64) -> Self {
    self.n = Some(n);
    self
  }

  /// Set the user identifier
  pub fn with_user(mut self, user: String) -> Self {
    self.user = Some(user);
    self
  }

  /// Set the quality (gpt-image-1 only)
  pub fn with_quality(mut self, quality: String) -> Self {
    self.quality = Some(quality);
    self
  }

  /// Set the background mode (gpt-image-1 only)
  pub fn with_background(mut self, background: String) -> Self {
    self.background = Some(background);
    self
  }

  /// Set the output format (gpt-image-1 only)
  pub fn with_output_format(mut self, format: String) -> Self {
    self.output_format = Some(format);
    self
  }

  /// Set the output compression (gpt-image-1 only)
  pub fn with_output_compression(mut self, compression: u64) -> Self {
    self.output_compression = Some(compression);
    self
  }

  /// Set the input fidelity (gpt-image-1 only)
  pub fn with_input_fidelity(mut self, fidelity: String) -> Self {
    self.input_fidelity = Some(fidelity);
    self
  }

  /// Set streaming mode (gpt-image-1 only)
  pub fn with_stream(mut self, stream: bool) -> Self {
    self.stream = Some(stream);
    self
  }
}

/// Token usage information for image generation (gpt-image-1 only)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Usage {
  pub total_tokens: i64,
  pub input_tokens: i64,
  pub output_tokens: i64,
  #[serde(default)]
  pub input_tokens_details: InputTokensDetails,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct InputTokensDetails {
  pub text_tokens: i64,
  pub image_tokens: i64,
}

/// Image edit wire 响应。
#[derive(Debug, Clone, Deserialize)]
pub struct ImageEditResponse {
  pub created: i64,
  pub data: Vec<ImageEditData>,
  /// The background parameter used for the image generation (gpt-image-1 only)
  #[serde(default)]
  pub background: Option<String>,
  /// The output format of the image generation (gpt-image-1 only)
  #[serde(default)]
  pub output_format: Option<String>,
  /// The quality of the image generated (gpt-image-1 only)
  #[serde(default)]
  pub quality: Option<String>,
  /// The size of the image generated
  #[serde(default)]
  pub size: Option<String>,
  /// Token usage information (gpt-image-1 only)
  #[serde(default)]
  pub usage: Option<Usage>,
}

/// 图像编辑终态：解码后的图像字节 + 原始 wire 响应。
#[derive(Debug, Clone)]
pub struct ImageEditResult {
  pub image: Vec<u8>,
  pub response: ImageEditResponse,
}

impl TryFrom<ImageEditResponse> for ImageEditResult {
  type Error = OpenAiCompatError;

  fn try_from(value: ImageEditResponse) -> Result<Self, Self::Error> {
    let first = value
      .data
      .first()
      .ok_or_else(|| OpenAiCompatError::ResponseParse("empty data array".into()))?;
    let url = first.url.as_str();
    let image = if url.is_empty() {
      // Decode from base64
      base64::prelude::BASE64_STANDARD
        .decode(&first.b64_json)
        .map_err(|e| OpenAiCompatError::ResponseParse(e.to_string()))?
    } else {
      // Download from URL
      log::info!("Download image from URL: {}", url);
      ureq::get(url)
        .call()
        .map_err(|e| OpenAiCompatError::ResponseParse(e.to_string()))?
        .into_body()
        .read_to_vec()
        .map_err(|e| OpenAiCompatError::ResponseParse(e.to_string()))?
    };

    Ok(ImageEditResult { image, response: value })
  }
}

// ================================================================
// Image Edit Model
// ================================================================

#[derive(Clone)]
pub struct ImageEditModel {
  client: Client,
  /// Name of the model (e.g.: qwen-image-edit, gpt-image-1, dall-e-2)
  pub model: String,
}

impl ImageEditModel {
  pub(crate) fn new(client: Client, model: &str) -> Self {
    Self { client, model: model.to_string() }
  }

  /// Build multipart form from request
  ///
  /// Note: We add multiple images using the same field name "image" multiple times.
  /// This is the standard way to send arrays in multipart/form-data and is compatible with:
  /// - OpenAI API (gpt-image-1, which supports up to 16 images)
  /// - Gitee AI API (various models, typically single image)
  /// - DALL-E 2 (single image only)
  fn build_form(&self, request: &ImageEditRequest) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new()
      .text("model", self.model.clone())
      .text("prompt", request.prompt.clone())
      .text("size", request.size.clone());

    // Add all images using the same field name "image" multiple times
    // This creates an array in multipart/form-data which is compatible with OpenAI and Gitee AI
    for (idx, image_data) in request.images.iter().enumerate() {
      let file_name = if request.images.len() == 1 { "image.png".to_string() } else { format!("image_{}.png", idx) };

      form = form.part(
        "image",
        reqwest::multipart::Part::bytes(image_data.clone())
          .file_name(file_name)
          .mime_str(mime::IMAGE_PNG.essence_str())
          .expect("image/png is a valid MIME type"),
      );
    }

    // Add optional mask (only for DALL-E 2 single image)
    if let Some(mask_data) = &request.mask_data {
      form = form.part(
        "mask",
        reqwest::multipart::Part::bytes(mask_data.clone())
          .file_name("mask.png")
          .mime_str(mime::IMAGE_PNG.essence_str())
          .expect("image/png is a valid MIME type"),
      );
    }

    // Add common parameters
    if let Some(n) = request.n {
      form = form.text("n", n.to_string());
    }
    if let Some(user) = &request.user {
      form = form.text("user", user.clone());
    }

    // Add gpt-image-1 specific parameters
    if self.model == "gpt-image-1" {
      if let Some(quality) = &request.quality {
        form = form.text("quality", quality.clone());
      }
      if let Some(background) = &request.background {
        form = form.text("background", background.clone());
      }
      if let Some(format) = &request.output_format {
        form = form.text("output_format", format.clone());
      }
      if let Some(compression) = request.output_compression {
        form = form.text("output_compression", compression.to_string());
      }
      if let Some(fidelity) = &request.input_fidelity {
        form = form.text("input_fidelity", fidelity.clone());
      }
      if let Some(partial) = request.partial_images {
        form = form.text("partial_images", partial.to_string());
      }
      if let Some(stream) = request.stream {
        form = form.text("stream", stream.to_string());
      }
    } else {
      // DALL-E 2 specific: response_format (default to b64_json)
      form = form.text("response_format", "b64_json");
    }

    form
  }

  /// Edit an image based on the provided request（multipart/form-data）。
  pub async fn image_edit(&self, request: ImageEditRequest) -> Result<ImageEditResult, OpenAiCompatError> {
    let form = self.build_form(&request);

    let response = self
      .client
      .http_client
      .post(self.client.endpoint("/images/edits"))
      .bearer_auth(&self.client.api_key)
      .multipart(form)
      .send()
      .await?;

    let status = response.status().as_u16();
    let body = response.bytes().await.map_err(|e| OpenAiCompatError::Transport(e.to_string()))?;

    if !(200..300).contains(&status) {
      let text = String::from_utf8_lossy(&body).to_string();
      return Err(OpenAiCompatError::Http { status, message: text });
    }

    match serde_json::from_slice::<ApiResponse<ImageEditResponse>>(&body).map_err(OpenAiCompatError::from)? {
      ApiResponse::Ok(response) => response.try_into(),
      ApiResponse::Err(err) => Err(err.into()),
    }
  }

  /// Create a builder for image edit requests
  pub fn edit_request(&self, images: Vec<Vec<u8>>, prompt: String, size: String) -> ImageEditRequest {
    ImageEditRequest::new(images, prompt, size)
  }

  /// Create a builder for single image edit requests (convenience method)
  pub fn edit_request_single(&self, image_data: Vec<u8>, prompt: String, size: String) -> ImageEditRequest {
    ImageEditRequest::new_single(image_data, prompt, size)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_image_edit_request_builder() {
    // Test single image with new_single method
    let request_single =
      ImageEditRequest::new_single(vec![1, 2, 3, 4], "test prompt".to_string(), "1024x1024".to_string())
        .with_quality("high".to_string())
        .with_background("transparent".to_string())
        .with_n(2);

    assert_eq!(request_single.images[0], vec![1, 2, 3, 4]);
    assert_eq!(request_single.prompt, "test prompt");
    assert_eq!(request_single.size, "1024x1024");
    assert_eq!(request_single.quality, Some("high".to_string()));
    assert_eq!(request_single.background, Some("transparent".to_string()));
    assert_eq!(request_single.n, Some(2));

    // Test multiple images with new method
    let request_multi = ImageEditRequest::new(
      vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]],
      "test prompt".to_string(),
      "1024x1024".to_string(),
    );

    assert_eq!(request_multi.images.len(), 2);
    assert_eq!(request_multi.images[0], vec![1, 2, 3, 4]);
    assert_eq!(request_multi.images[1], vec![5, 6, 7, 8]);
    assert_eq!(request_multi.prompt, "test prompt");
    assert_eq!(request_multi.size, "1024x1024");
  }
}
