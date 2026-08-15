//! OpenAI 兼容客户端。
//!
//! # Example
//! ```
//! use fusion_ai::providers::openai_compatible::Client;
//!
//! let client = Client::new("YOUR_API_KEY");
//! // 默认 Responses 形态；仅支持 chat completions 的端点（Moonshot 等）走 chat_completions_model
//! let model = client.chat_completions_model("moonshot-v1-8k");
//! ```

use serde::Deserialize;

use crate::providers::openai_compatible::errors::OpenAiCompatError;
use crate::providers::openai_compatible::CompletionModel;

use super::embedding::{EmbeddingModel, TEXT_EMBEDDING_3_LARGE, TEXT_EMBEDDING_3_SMALL, TEXT_EMBEDDING_ADA_002};
use super::transcription::TranscriptionModel;

#[cfg(feature = "image")]
use super::image_edit::ImageEditModel;
#[cfg(feature = "image")]
use super::image_generation::ImageGenerationModel;

#[cfg(feature = "audio")]
use super::audio_generation::AudioGenerationModel;

// ================================================================
// Main OpenAI Client
// ================================================================
const OPENAI_API_BASE_URL: &str = "https://api.openai.com/v1";

pub struct ClientBuilder<'a> {
  api_key: &'a str,
  base_url: &'a str,
  http_client: reqwest::Client,
}

impl<'a> ClientBuilder<'a> {
  pub fn new(api_key: &'a str) -> Self {
    Self { api_key, base_url: OPENAI_API_BASE_URL, http_client: Default::default() }
  }

  pub fn new_with_client(api_key: &'a str, http_client: reqwest::Client) -> Self {
    ClientBuilder { api_key, base_url: OPENAI_API_BASE_URL, http_client }
  }

  pub fn base_url(mut self, base_url: &'a str) -> Self {
    self.base_url = base_url;
    self
  }

  pub fn with_client(mut self, http_client: reqwest::Client) -> Self {
    self.http_client = http_client;
    self
  }

  pub fn build(self) -> Client {
    Client { base_url: self.base_url.to_string(), api_key: self.api_key.to_string(), http_client: self.http_client }
  }
}

/// OpenAI 兼容客户端（具体 `reqwest::Client`，不泛型化 HTTP 后端）。
#[derive(Clone)]
pub struct Client {
  pub(crate) base_url: String,
  pub(crate) api_key: String,
  pub(crate) http_client: reqwest::Client,
}

impl Debug for Client {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Client")
      .field("base_url", &self.base_url)
      .field("api_key", &"<REDACTED>")
      .finish()
  }
}

use std::fmt::Debug;

impl Client {
  /// Create a new OpenAI client builder.
  ///
  /// # Example
  /// ```
  /// use fusion_ai::providers::openai_compatible::Client;
  ///
  /// let openai_client = Client::builder("your-open-ai-api-key").build();
  /// ```
  pub fn builder(api_key: &str) -> ClientBuilder<'_> {
    ClientBuilder::new(api_key)
  }

  /// Create a new OpenAI client. For more control, use the `builder` method.
  pub fn new(api_key: &str) -> Self {
    Self::builder(api_key).build()
  }

  /// Create a new OpenAI client from environment variables.
  /// Panics if OPENAI_API_KEY is not set.
  pub fn from_env() -> Self {
    let base_url: Option<String> = std::env::var("OPENAI_BASE_URL").ok();
    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set");

    match base_url {
      Some(url) => ClientBuilder::new(&api_key).base_url(&url).build(),
      None => ClientBuilder::new(&api_key).build(),
    }
  }

  pub(crate) fn endpoint(&self, path: &str) -> String {
    format!("{}/{}", self.base_url, path.trim_start_matches('/'))
  }

  /// 原生 reqwest POST（JSON body + bearer auth）——本地化 wire 路径统一入口。
  pub(crate) fn post_json(&self, path: &str, body: Vec<u8>) -> reqwest::RequestBuilder {
    self.http_client
      .post(self.endpoint(path))
      .bearer_auth(&self.api_key)
      .header("Content-Type", "application/json")
      .body(body)
  }

  /// 原生 reqwest GET（bearer auth）。
  pub(crate) fn get_bearer(&self, path: &str) -> reqwest::RequestBuilder {
    self.http_client.get(self.endpoint(path)).bearer_auth(&self.api_key)
  }

  /// 非 2xx 响应统一转 [`OpenAiCompatError::Http`]（status 保留瞬态分级信息）。
  pub(crate) async fn error_from_response(response: reqwest::Response) -> OpenAiCompatError {
    let status = response.status().as_u16();
    let message = response.text().await.unwrap_or_default();
    OpenAiCompatError::Http { status, message }
  }

  /// 凭证探测：GET /models。
  ///
  /// 401 → `Http { status: 401 }`（凭证无效）；5xx → `Http`（上游瞬态）；
  /// 其余非 2xx 按现状语义宽松放行（Ok）。
  pub async fn verify(&self) -> Result<(), OpenAiCompatError> {
    let response = self.get_bearer("/models").send().await?;

    match response.status().as_u16() {
      200..=299 => Ok(()),
      401 => Err(OpenAiCompatError::Http {
        status: 401,
        message: "Invalid authentication".to_string(),
      }),
      status if status >= 500 => {
        let message = response.text().await.unwrap_or_default();
        Err(OpenAiCompatError::Http { status, message })
      }
      _ => Ok(()),
    }
  }
}

impl Client {
  // —— 具体 model 工厂方法（原 rig client trait 的本地化形态）——

  /// 默认 Responses 形态的 completion model；Chat Completions 经 `.completions_api()` 切换。
  ///
  /// # Example
  /// ```
  /// use fusion_ai::providers::openai_compatible::Client;
  ///
  /// let openai = Client::new("your-open-ai-api-key");
  /// let model = openai.completion_model("gpt-4o");
  /// ```
  pub fn completion_model(&self, model: impl Into<String>) -> super::responses_api::ResponsesCompletionModel {
    let model = model.into();
    super::responses_api::ResponsesCompletionModel::new(self.clone(), &model)
  }

  /// Chat Completions 形态的 completion model（Moonshot 等仅支持 chat completions 的端点）。
  ///
  /// # Example
  /// ```
  /// use fusion_ai::providers::openai_compatible::Client;
  ///
  /// let openai = Client::new("your-open-ai-api-key");
  /// let chat = openai.chat_completions_model("moonshot-v1-8k");
  /// ```
  pub fn chat_completions_model(&self, model: impl Into<String>) -> CompletionModel {
    let model = model.into();
    CompletionModel::new(self.clone(), &model)
  }

  pub fn embedding_model(&self, model: impl Into<String>) -> EmbeddingModel {
    let model_str = model.into();
    let ndims = match model_str.as_str() {
      TEXT_EMBEDDING_3_LARGE => 3072,
      TEXT_EMBEDDING_3_SMALL | TEXT_EMBEDDING_ADA_002 => 1536,
      _ => 0,
    };
    EmbeddingModel::new(self.clone(), &model_str, ndims)
  }

  pub fn embedding_model_with_ndims(&self, model: impl Into<String>, ndims: usize) -> EmbeddingModel {
    EmbeddingModel::new(self.clone(), model.into().as_str(), ndims)
  }

  /// Create a transcription model with the given name.
  pub fn transcription_model(&self, model: impl Into<String>) -> TranscriptionModel {
    let model = model.into();
    TranscriptionModel::new(self.clone(), &model)
  }

  /// Create an image generation model with the given name.
  #[cfg(feature = "image")]
  pub fn image_generation_model(&self, model: impl Into<String>) -> ImageGenerationModel {
    let model = model.into();
    ImageGenerationModel::new(self.clone(), &model)
  }

  /// Create an image edit model with the given name.
  #[cfg(feature = "image")]
  pub fn image_edit_model(&self, model: &str) -> ImageEditModel {
    ImageEditModel::new(self.clone(), model)
  }

  /// Create an audio generation model with the given name.
  #[cfg(feature = "audio")]
  pub fn audio_generation_model(&self, model: impl Into<String>) -> AudioGenerationModel {
    let model = model.into();
    AudioGenerationModel::new(self.clone(), &model)
  }

}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiErrorResponse {
  pub(crate) message: String,
}

/// 2xx body 内的 `{ "message": "..." }` 错误体（OpenAI 兼容端少见但有此方言）。
/// status 用 200 占位：真实非 2xx 走 [`OpenAiCompatError::Http`] 路径。
impl From<ApiErrorResponse> for OpenAiCompatError {
  fn from(err: ApiErrorResponse) -> Self {
    OpenAiCompatError::Http { status: 200, message: err.message }
  }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ApiResponse<T> {
  Ok(T),
  Err(ApiErrorResponse),
}

#[cfg(test)]
mod tests {
  use crate::providers::openai_compatible::types::OneOrMany;
  use crate::providers::openai_compatible::{
    AssistantContent, Client, Function, ImageUrl, Message, ToolCall, ToolType, UserContent as WireUserContent,
    try_from_message_to_vec_input_item,
  };

  #[test]
  fn test_deserialize_message() {
    let assistant_message_json = r#"
        {
            "role": "assistant",
            "content": "\n\nHello there, how may I assist you today?"
        }
        "#;

    let assistant_message_json2 = r#"
        {
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "\n\nHello there, how may I assist you today?"
                }
            ],
            "tool_calls": null
        }
        "#;

    let assistant_message_json3 = r#"
        {
            "role": "assistant",
            "tool_calls": [
                {
                    "id": "call_h89ipqYUjEpCPI6SxspMnoUU",
                    "type": "function",
                    "function": {
                        "name": "subtract",
                        "arguments": "{\"x\": 2, \"y\": 5}"
                    }
                }
            ],
            "content": null,
            "refusal": null
        }
        "#;

    let user_message_json = r#"
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "What's in this image?"
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "https://upload.wikimedia.org/wikipedia/commons/thumb/d/dd/Gfp-wisconsin-madison-the-nature-boardwalk.jpg/2560px-Gfp-wisconsin-madison-the-nature-boardwalk.jpg"
                    }
                },
                {
                    "type": "audio",
                    "input_audio": {
                        "data": "...",
                        "format": "mp3"
                    }
                }
            ]
        }
        "#;

    let assistant_message: Message = {
      let jd = &mut serde_json::Deserializer::from_str(assistant_message_json);
      serde_path_to_error::deserialize(jd).unwrap_or_else(|err| {
        panic!("Deserialization error at {} ({}:{}): {}", err.path(), err.inner().line(), err.inner().column(), err);
      })
    };

    let assistant_message2: Message = {
      let jd = &mut serde_json::Deserializer::from_str(assistant_message_json2);
      serde_path_to_error::deserialize(jd).unwrap_or_else(|err| {
        panic!("Deserialization error at {} ({}:{}): {}", err.path(), err.inner().line(), err.inner().column(), err);
      })
    };

    let assistant_message3: Message = {
      let jd: &mut serde_json::Deserializer<serde_json::de::StrRead<'_>> =
        &mut serde_json::Deserializer::from_str(assistant_message_json3);
      serde_path_to_error::deserialize(jd).unwrap_or_else(|err| {
        panic!("Deserialization error at {} ({}:{}): {}", err.path(), err.inner().line(), err.inner().column(), err);
      })
    };

    let user_message: Message = {
      let jd = &mut serde_json::Deserializer::from_str(user_message_json);
      serde_path_to_error::deserialize(jd).unwrap_or_else(|err| {
        panic!("Deserialization error at {} ({}:{}): {}", err.path(), err.inner().line(), err.inner().column(), err);
      })
    };

    match assistant_message {
      Message::Assistant { content, .. } => {
        assert_eq!(
          content[0],
          AssistantContent::Text { text: "\n\nHello there, how may I assist you today?".to_string() }
        );
      }
      _ => panic!("Expected assistant message"),
    }

    match assistant_message2 {
      Message::Assistant { content, tool_calls, .. } => {
        assert_eq!(
          content[0],
          AssistantContent::Text { text: "\n\nHello there, how may I assist you today?".to_string() }
        );

        assert_eq!(tool_calls, vec![]);
      }
      _ => panic!("Expected assistant message"),
    }

    match assistant_message3 {
      Message::Assistant { content, tool_calls, refusal, .. } => {
        assert!(content.is_empty());
        assert!(refusal.is_none());
        assert_eq!(
          tool_calls[0],
          ToolCall {
            id: "call_h89ipqYUjEpCPI6SxspMnoUU".to_string(),
            r#type: ToolType::Function,
            function: Function { name: "subtract".to_string(), arguments: serde_json::json!({"x": 2, "y": 5}) },
            signature: None,
            additional_params: None,
          }
        );
      }
      _ => panic!("Expected assistant message"),
    }

    match user_message {
      Message::User { content, .. } => {
        let (first, second) = {
          let mut iter = content.into_iter();
          (iter.next().unwrap(), iter.next().unwrap())
        };
        assert_eq!(first, WireUserContent::Text { text: "What's in this image?".to_string() });
        assert_eq!(
          second,
          WireUserContent::Image {
            image_url: ImageUrl {
              url: "https://upload.wikimedia.org/wikipedia/commons/thumb/d/dd/Gfp-wisconsin-madison-the-nature-boardwalk.jpg/2560px-Gfp-wisconsin-madison-the-nature-boardwalk.jpg".to_string(),
              detail: crate::providers::openai_compatible::types::ImageDetail::default()
            }
          }
        );
      }
      _ => panic!("Expected user message"),
    }
  }

  #[test]
  fn test_message_to_message_conversion() {
    use crate::providers::openai_compatible::types as core;

    let user_message = core::Message::User { content: OneOrMany::one(core::UserContent::text("Hello")) };

    let assistant_message = core::Message::Assistant {
      id: None,
      content: OneOrMany::one(core::AssistantContent::text("Hi there!")),
    };

    let converted_user_message: Vec<Message> = try_from_message_to_vec_input_item(user_message.clone()).unwrap();
    let converted_assistant_message: Vec<Message> =
      try_from_message_to_vec_input_item(assistant_message.clone()).unwrap();

    match converted_user_message[0].clone() {
      Message::User { content, .. } => {
        assert_eq!(content.first(), WireUserContent::Text { text: "Hello".to_string() });
      }
      _ => panic!("Expected user message"),
    }

    match converted_assistant_message[0].clone() {
      Message::Assistant { content, .. } => {
        assert_eq!(content[0].clone(), AssistantContent::Text { text: "Hi there!".to_string() });
      }
      _ => panic!("Expected assistant message"),
    }
  }

  #[test]
  fn test_message_from_message_conversion() {
    use crate::providers::openai_compatible::types as core;

    let user_message = Message::User { content: OneOrMany::one(WireUserContent::Text { text: "Hello".to_string() }), name: None };

    let assistant_message = Message::Assistant {
      content: vec![AssistantContent::Text { text: "Hi there!".to_string() }],
      refusal: None,
      audio: None,
      name: None,
      tool_calls: vec![],
    };

    let converted_user_message: core::Message = user_message.clone().try_into().unwrap();
    let converted_assistant_message: core::Message = assistant_message.clone().try_into().unwrap();

    match converted_user_message.clone() {
      core::Message::User { content } => {
        assert_eq!(content.first(), core::UserContent::text("Hello"));
      }
      _ => panic!("Expected user message"),
    }

    match converted_assistant_message.clone() {
      core::Message::Assistant { content, .. } => {
        assert_eq!(content.first(), core::AssistantContent::text("Hi there!"));
      }
      _ => panic!("Expected assistant message"),
    }

    let original_user_message: Vec<Message> =
      try_from_message_to_vec_input_item(converted_user_message.clone()).unwrap();
    let original_assistant_message: Vec<Message> =
      try_from_message_to_vec_input_item(converted_assistant_message.clone()).unwrap();

    assert_eq!(original_user_message[0], user_message);
    assert_eq!(original_assistant_message[0], assistant_message);
  }

  #[test]
  fn client_debug_never_leaks_api_key() {
    let client = Client::new("sk-test-secret");
    let dbg = format!("{client:?}");
    assert!(!dbg.contains("sk-test-secret"));
    assert!(dbg.contains("REDACTED"));
  }
}
