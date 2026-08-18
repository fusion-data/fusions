// ================================================================
// OpenAI Completion API（Chat Completions，类型本地化）
// ================================================================
use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tracing::{Instrument, info_span};

use crate::json_utils;
use crate::providers::openai_compatible::errors::OpenAiCompatError;
use crate::providers::openai_compatible::types::{self as core, OneOrMany};
use crate::providers::openai_compatible::{ApiResponse, Client};

pub mod streaming;

// 模型常量（fork 自 rig providers::openai::completion，演进自主）
pub const GPT_4: &str = "gpt-4";
pub const GPT_4_32K: &str = "gpt-4-32k";
pub const GPT_4_32K_0613: &str = "gpt-4-32k-0613";
pub const GPT_4_0613: &str = "gpt-4-0613";
pub const GPT_4_1106_PREVIEW: &str = "gpt-4-1106-preview";
pub const GPT_4_0125_PREVIEW: &str = "gpt-4-0125-preview";
pub const GPT_4_TURBO_PREVIEW: &str = "gpt-4-turbo-preview";
pub const GPT_4_TURBO: &str = "gpt-4-turbo";
pub const GPT_4_TURBO_2024_04_09: &str = "gpt-4-turbo-2024-04-09";
pub const GPT_4_1106_VISION_PREVIEW: &str = "gpt-4-1106-vision-preview";
pub const GPT_4_VISION_PREVIEW: &str = "gpt-4-vision-preview";
pub const GPT_4O: &str = "gpt-4o";
pub const GPT_4O_2024_05_13: &str = "gpt-4o-2024-05-13";
pub const GPT_4O_2024_11_20: &str = "gpt-4o-2024-11-20";
pub const GPT_4O_MINI: &str = "gpt-4o-mini";
pub const GPT_4_1: &str = "gpt-4.1";
pub const GPT_4_1_2025_04_14: &str = "gpt-4.1-2025-04-14";
pub const GPT_4_1_MINI: &str = "gpt-4.1-mini";
pub const GPT_4_1_NANO: &str = "gpt-4.1-nano";
pub const GPT_4_5_PREVIEW: &str = "gpt-4.5-preview";
pub const GPT_4_5_PREVIEW_2025_02_27: &str = "gpt-4.5-preview-2025-02-27";
pub const O1: &str = "o1";
pub const O1_2024_12_17: &str = "o1-2024-12-17";
pub const O1_MINI: &str = "o1-mini";
pub const O1_MINI_2024_09_12: &str = "o1-mini-2024-09-12";
pub const O1_PREVIEW: &str = "o1-preview";
pub const O1_PREVIEW_2024_09_12: &str = "o1-preview-2024-09-12";
pub const O1_PRO: &str = "o1-pro";
pub const O3: &str = "o3";
pub const O3_MINI: &str = "o3-mini";
pub const O3_MINI_2025_01_31: &str = "o3-mini-2025-01-31";
pub const O4_MINI: &str = "o4-mini";
pub const O4_MINI_2025_04_16: &str = "o4-mini-2025-04-16";

/// Chat Completions wire 消息。
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
  #[serde(alias = "developer")]
  System {
    #[serde(deserialize_with = "core::string_or_one_or_many")]
    content: OneOrMany<SystemContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
  },
  User {
    #[serde(deserialize_with = "core::string_or_one_or_many")]
    content: OneOrMany<UserContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
  },
  Assistant {
    // DashScope 兼容端要求 assistant content 字段恒序列化（缺失 → 400），
    // 因此不用 skip_serializing_if
    #[serde(default, deserialize_with = "json_utils::string_or_vec")]
    content: Vec<AssistantContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<AudioAssistant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, deserialize_with = "json_utils::null_or_vec", skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
  },
  #[serde(rename = "tool")]
  ToolResult { tool_call_id: String, content: OneOrMany<ToolResultContent> },
}

impl Message {
  pub fn system(content: &str) -> Self {
    Message::System { content: OneOrMany::one(content.to_owned().into()), name: None }
  }

  pub fn user(content: &str) -> Self {
    Message::User { content: OneOrMany::one(content.to_owned().into()), name: None }
  }

  pub fn assistant(content: &str) -> Self {
    Message::Assistant {
      content: vec![AssistantContent::Text { text: content.to_owned() }],
      refusal: None,
      audio: None,
      name: None,
      tool_calls: vec![],
    }
  }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AudioAssistant {
  pub id: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SystemContent {
  #[serde(default)]
  pub r#type: SystemContentType,
  pub text: String,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum SystemContentType {
  #[default]
  Text,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AssistantContent {
  Text { text: String },
  Refusal { refusal: String },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UserContent {
  Text {
    text: String,
  },
  #[serde(rename = "image_url")]
  Image {
    image_url: ImageUrl,
  },
  Audio {
    input_audio: InputAudio,
  },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImageUrl {
  pub url: String,
  #[serde(default)]
  pub detail: core::ImageDetail,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InputAudio {
  pub data: String,
  pub format: core::AudioMediaType,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultContent {
  #[serde(default)]
  r#type: ToolResultContentType,
  pub text: String,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultContentType {
  #[default]
  Text,
}

impl FromStr for ToolResultContent {
  type Err = Infallible;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(s.to_owned().into())
  }
}

impl From<String> for ToolResultContent {
  fn from(s: String) -> Self {
    ToolResultContent { r#type: ToolResultContentType::default(), text: s }
  }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
  pub id: String,
  #[serde(default)]
  pub r#type: ToolType,
  pub function: Function,
  pub signature: Option<String>,
  pub additional_params: Option<serde_json::Value>,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
  #[default]
  Function,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
  pub r#type: String,
  pub function: core::ToolDefinition,
}

impl From<core::ToolDefinition> for ToolDefinition {
  fn from(tool: core::ToolDefinition) -> Self {
    Self { r#type: "function".into(), function: tool }
  }
}

#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
  #[default]
  Auto,
  None,
  Required,
}

impl TryFrom<core::ToolChoice> for ToolChoice {
  type Error = OpenAiCompatError;
  fn try_from(value: core::ToolChoice) -> Result<Self, Self::Error> {
    let res = match value {
      core::ToolChoice::Specific { .. } => {
        return Err(OpenAiCompatError::request_build("Provider doesn't support only using specific tools"));
      }
      core::ToolChoice::Auto => Self::Auto,
      core::ToolChoice::None => Self::None,
      core::ToolChoice::Required => Self::Required,
    };

    Ok(res)
  }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Function {
  pub name: String,
  #[serde(with = "json_utils::stringified_json")]
  pub arguments: serde_json::Value,
}

/// 内部消息模型 → Chat Completions wire 消息。
pub fn try_from_message_to_vec_input_item(message: core::Message) -> Result<Vec<Message>, core::MessageError> {
  use core::{DocumentSourceKind, Message as CoreMessage};

  match message {
    CoreMessage::User { content } => {
      let (tool_results, other_content): (Vec<_>, Vec<_>) =
        content.into_iter().partition(|content| matches!(content, core::UserContent::ToolResult(_)));

      // If there are messages with both tool results and user content, openai will only
      //  handle tool results. It's unlikely that there will be both.
      if !tool_results.is_empty() {
        tool_results
          .into_iter()
          .map(|content| match content {
            core::UserContent::ToolResult(core::ToolResult { id, content, call_id }) => {
              let tool_call_id = call_id.unwrap_or(id);
              Ok::<_, core::MessageError>(Message::ToolResult {
                tool_call_id,
                content: {
                  let items: Vec<_> = content.into_iter().collect();
                  let mapped_items: Result<Vec<_>, _> = items
                    .into_iter()
                    .map(|content| match content {
                      core::ToolResultContent::Text(text) => Ok(text.text.into()),
                      _ => Err(core::MessageError::conversion("Tool result content does not support non-text")),
                    })
                    .collect();
                  let mapped_items = mapped_items?;
                  if mapped_items.len() == 1 {
                    OneOrMany::one(mapped_items.into_iter().next().unwrap())
                  } else {
                    OneOrMany::many(mapped_items)
                      .map_err(|_| core::MessageError::conversion("Failed to create OneOrMany from mapped items"))?
                  }
                },
              })
            }
            _ => unreachable!(),
          })
          .collect::<Result<Vec<_>, _>>()
      } else {
        let other_content: Vec<UserContent> = other_content
          .into_iter()
          .map(|content| match content {
            core::UserContent::Text(text) => Ok(UserContent::Text { text: text.text }),
            core::UserContent::Image(core::Image { data, detail, media_type, .. }) => match data {
              DocumentSourceKind::Url(url) => {
                Ok(UserContent::Image { image_url: ImageUrl { url, detail: detail.unwrap_or_default() } })
              }
              DocumentSourceKind::Base64(data) => {
                let url = format!(
                  "data:{};base64,{}",
                  media_type
                    .map(|i| i.to_mime_type())
                    .ok_or_else(|| core::MessageError::conversion("OpenAI Image URI must have media type"))?,
                  data
                );

                let detail =
                  detail.ok_or_else(|| core::MessageError::conversion("OpenAI image URI must have image detail"))?;

                Ok(UserContent::Image { image_url: ImageUrl { url, detail } })
              }
              DocumentSourceKind::Raw(_) => {
                Err(core::MessageError::conversion("Raw files not supported, encode as base64 first"))
              }
              DocumentSourceKind::String(_) | DocumentSourceKind::Unknown => {
                Err(core::MessageError::conversion("Document has no supported body"))
              }
            },
            core::UserContent::Document(core::Document { data, .. }) => {
              if let DocumentSourceKind::Base64(text) | DocumentSourceKind::String(text) = data {
                Ok(UserContent::Text { text })
              } else {
                Err(core::MessageError::conversion("Documents must be base64 or a string"))
              }
            }
            core::UserContent::Audio(core::Audio { data: DocumentSourceKind::Base64(data), media_type, .. }) => {
              Ok(UserContent::Audio {
                input_audio: InputAudio {
                  data,
                  format: match media_type {
                    Some(media_type) => media_type,
                    None => core::AudioMediaType::MP3,
                  },
                },
              })
            }
            _ => Err(core::MessageError::conversion("Tool result is in unsupported format")),
          })
          .collect::<Result<Vec<_>, _>>()?;

        let other_content = OneOrMany::many(other_content)
          .expect("There must be other content here if there were no tool result content");

        Ok(vec![Message::User { content: other_content, name: None }])
      }
    }
    CoreMessage::System { content } => Ok(vec![Message::System {
      content: OneOrMany::one(SystemContent { r#type: Default::default(), text: content }),
      name: None,
    }]),
    CoreMessage::Assistant { content, .. } => {
      let (text_content, tool_calls): (Vec<_>, Vec<_>) =
        content.into_iter().try_fold((Vec::new(), Vec::new()), |(mut texts, mut tools), content| {
          match content {
            core::AssistantContent::Text(text) => texts.push(text),
            core::AssistantContent::ToolCall(tool_call) => tools.push(tool_call),
            core::AssistantContent::Reasoning(_) => {
              return Err(core::MessageError::conversion("OpenAI Completions API does not support reasoning content"));
            }
            core::AssistantContent::Image(_) => {
              return Err(core::MessageError::conversion("OpenAI Completions API does not support image content"));
            }
          }
          Ok((texts, tools))
        })?;

      // `OneOrMany` ensures at least one `AssistantContent::Text` or `ToolCall` exists,
      //  so either `content` or `tool_calls` will have some content.
      Ok(vec![Message::Assistant {
        content: text_content.into_iter().map(|content| content.text.into()).collect::<Vec<_>>(),
        refusal: None,
        audio: None,
        name: None,
        tool_calls: tool_calls.into_iter().map(|tool_call| tool_call.into()).collect::<Vec<_>>(),
      }])
    }
  }
}

impl From<core::ToolCall> for ToolCall {
  fn from(tool_call: core::ToolCall) -> Self {
    Self {
      id: tool_call.id,
      r#type: ToolType::default(),
      function: Function { name: tool_call.function.name, arguments: tool_call.function.arguments },
      signature: tool_call.signature,
      additional_params: tool_call.additional_params,
    }
  }
}

impl From<ToolCall> for core::ToolCall {
  fn from(tool_call: ToolCall) -> Self {
    Self {
      id: tool_call.id,
      call_id: None,
      function: core::ToolFunction { name: tool_call.function.name, arguments: tool_call.function.arguments },
      signature: tool_call.signature,
      additional_params: tool_call.additional_params,
    }
  }
}

impl TryFrom<Message> for core::Message {
  type Error = core::MessageError;

  fn try_from(message: Message) -> Result<Self, Self::Error> {
    Ok(match message {
      Message::User { content, .. } => {
        let mapped_content: Vec<_> = content.into_iter().map(|content| content.into()).collect();
        let new_content = if mapped_content.len() == 1 {
          OneOrMany::one(mapped_content.into_iter().next().unwrap())
        } else {
          OneOrMany::many(mapped_content)
            .map_err(|_| core::MessageError::conversion("Failed to create OneOrMany from content"))?
        };
        core::Message::User { content: new_content }
      }
      Message::Assistant { content, tool_calls, .. } => {
        let mut content = content
          .into_iter()
          .map(|content| match content {
            AssistantContent::Text { text } => core::AssistantContent::text(text),
            // Refusal 目前降级为 text（沿用 fork 基线行为）
            AssistantContent::Refusal { refusal } => core::AssistantContent::text(refusal),
          })
          .collect::<Vec<_>>();

        content.extend(
          tool_calls
            .into_iter()
            .map(|tool_call| core::AssistantContent::ToolCall(tool_call.into()))
            .collect::<Vec<_>>(),
        );

        core::Message::Assistant {
          id: None,
          content: OneOrMany::many(content).map_err(|_| {
            core::MessageError::conversion("Neither `content` nor `tool_calls` was provided to the Message")
          })?,
        }
      }

      Message::ToolResult { tool_call_id, content } => core::Message::User {
        content: OneOrMany::one(core::UserContent::ToolResult(core::ToolResult {
          id: tool_call_id,
          call_id: None,
          content: {
            let items: Vec<_> = content.into_iter().collect();
            let mapped_items: Vec<_> =
              items.into_iter().map(|content| core::ToolResultContent::text(content.text)).collect();
            if mapped_items.len() == 1 {
              OneOrMany::one(mapped_items.into_iter().next().unwrap())
            } else {
              OneOrMany::many(mapped_items)
                .map_err(|_| core::MessageError::conversion("Failed to create OneOrMany from mapped items"))?
            }
          },
        })),
      },

      // System messages should get stripped out when converting messages, this is just a
      // stop gap to avoid obnoxious error handling or panic occurring.
      Message::System { content, .. } => {
        let items: Vec<_> = content.into_iter().collect();
        let mapped_items: Vec<_> = items.into_iter().map(|content| core::UserContent::text(content.text)).collect();
        let content = if mapped_items.len() == 1 {
          OneOrMany::one(mapped_items.into_iter().next().unwrap())
        } else {
          OneOrMany::many(mapped_items)
            .map_err(|_| core::MessageError::conversion("Failed to create OneOrMany from mapped items"))?
        };
        core::Message::User { content }
      }
    })
  }
}

impl From<UserContent> for core::UserContent {
  fn from(content: UserContent) -> Self {
    match content {
      UserContent::Text { text } => core::UserContent::text(text),
      UserContent::Image { image_url } => core::UserContent::image_url(image_url.url, None, Some(image_url.detail)),
      UserContent::Audio { input_audio } => core::UserContent::audio(input_audio.data, Some(input_audio.format)),
    }
  }
}

impl From<String> for UserContent {
  fn from(s: String) -> Self {
    UserContent::Text { text: s }
  }
}

impl FromStr for UserContent {
  type Err = Infallible;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(UserContent::Text { text: s.to_string() })
  }
}

impl From<String> for AssistantContent {
  fn from(s: String) -> Self {
    AssistantContent::Text { text: s }
  }
}

impl FromStr for AssistantContent {
  type Err = Infallible;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(AssistantContent::Text { text: s.to_string() })
  }
}

impl From<String> for SystemContent {
  fn from(s: String) -> Self {
    SystemContent { r#type: SystemContentType::default(), text: s }
  }
}

impl FromStr for SystemContent {
  type Err = Infallible;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(SystemContent { r#type: SystemContentType::default(), text: s.to_string() })
  }
}

// wire 方言是 OpenAI snake_case（system_fingerprint / finish_reason），
// 不得 rename 成 camelCase，否则真实端点响应反序列化失败
#[derive(Debug, Deserialize, Serialize)]
pub struct CompletionResponse {
  pub id: String,
  pub object: Option<String>,
  pub created: u64,
  pub model: String,
  pub system_fingerprint: Option<String>,
  pub choices: Vec<Choice>,
  pub usage: Option<Usage>,
}

impl CompletionResponse {
  /// 首个 assistant 选择的消息。
  pub fn assistant_message(&self) -> Option<&Message> {
    self.choices.first().map(|choice| &choice.message)
  }

  /// 首个 assistant 消息的拼接文本（Refusal 计入文本）。
  pub fn text(&self) -> Option<String> {
    match self.assistant_message()? {
      Message::Assistant { content, .. } => {
        let text = content
          .iter()
          .map(|c| match c {
            AssistantContent::Text { text } => text.as_str(),
            AssistantContent::Refusal { refusal } => refusal.as_str(),
          })
          .collect::<Vec<_>>()
          .join("");
        Some(text)
      }
      _ => None,
    }
  }

  /// 工具调用列表（首个选择）。
  pub fn tool_calls(&self) -> &[ToolCall] {
    match self.assistant_message() {
      Some(Message::Assistant { tool_calls, .. }) => tool_calls,
      _ => &[],
    }
  }

  /// 通用 token 用量（provider 无关形态）。
  pub fn usage_tokens(&self) -> core::Usage {
    match &self.usage {
      Some(usage) => core::Usage {
        input_tokens: usage.prompt_tokens as u64,
        output_tokens: usage.total_tokens.saturating_sub(usage.prompt_tokens) as u64,
        total_tokens: usage.total_tokens as u64,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: 0,
      },
      None => core::Usage::default(),
    }
  }
}

impl fmt::Display for Usage {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    let Usage { prompt_tokens, completion_tokens: _, total_tokens, .. } = self;
    write!(f, "Prompt tokens: {prompt_tokens} Total tokens: {total_tokens}")
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Choice {
  pub index: usize,
  pub message: Message,
  pub logprobs: Option<serde_json::Value>,
  pub finish_reason: String,
}

/// cache 命中数判据**单点** —— 双方言解析的唯一实现：
///
/// - DeepSeek flat `prompt_cache_hit_tokens`
/// - OpenAI 嵌套 `prompt_tokens_details.cached_tokens`
///
/// 厂商只回其一；同时出现取 max（防御性，避免同 token 双计）；皆无 → 0（退化为不拆分口径）。
/// 输入是**原始 usage JSON**（不依赖任何一方 wire 的本地类型），[`Usage`] 的
/// `Deserialize` 与 `llm::wire_openai_compat` 的 wire 用量都 MUST 经本函数取值 ——
/// 两套 OpenAI 兼容 wire 同源由「函数只有一个」兜底，不靠审查纪律。
pub(crate) fn cache_hit_input_tokens(usage: &serde_json::Value) -> u64 {
  let flat = usage.get("prompt_cache_hit_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0);
  let nested = usage
    .pointer("/prompt_tokens_details/cached_tokens")
    .and_then(serde_json::Value::as_u64)
    .unwrap_or(0);
  flat.max(nested)
}

/// Chat Completions wire 用量（OpenAI 方言：prompt_tokens / completion_tokens / total_tokens）。
///
/// `cached_input_tokens` 在 [`Deserialize`] 时即从原始 usage JSON 经
/// [`cache_hit_input_tokens`]（判据单点）求值落字段，后续读取零解析成本。
#[derive(Clone, Debug, Serialize)]
pub struct Usage {
  #[serde(default)]
  pub prompt_tokens: usize,
  #[serde(default)]
  pub completion_tokens: usize,
  #[serde(default)]
  pub total_tokens: usize,
  /// cache 命中 input tokens（双方言判据见 [`cache_hit_input_tokens`]；皆无 → 0）。
  #[serde(default)]
  pub cached_input_tokens: u64,
}

impl<'de> Deserialize<'de> for Usage {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Base {
      prompt_tokens: usize,
      completion_tokens: usize,
      total_tokens: usize,
    }
    let raw = serde_json::Value::deserialize(deserializer)?;
    let base: Base = serde_json::from_value(raw.clone()).map_err(serde::de::Error::custom)?;
    Ok(Self {
      prompt_tokens: base.prompt_tokens,
      completion_tokens: base.completion_tokens,
      total_tokens: base.total_tokens,
      cached_input_tokens: cache_hit_input_tokens(&raw),
    })
  }
}

impl Usage {
  pub fn new() -> Self {
    Self { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, cached_input_tokens: 0 }
  }
}

impl Default for Usage {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Clone)]
pub struct CompletionModel {
  pub(crate) client: Client,
  /// Name of the model (e.g.: gpt-3.5-turbo-1106)
  pub model: String,
}

impl CompletionModel {
  pub fn new(client: Client, model: &str) -> Self {
    Self { client, model: model.to_string() }
  }

  pub fn model(&self) -> &str {
    &self.model
  }
}

/// Chat Completions 请求（公共构造面）。
///
/// `additional_params` 是 extra-body 注入点：thinking 关闭（DeepSeek
/// `{"thinking":{"type":"disabled"}}`、Qwen `{"enable_thinking":false}`）等
/// provider 专属参数经此展开进请求体。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CompletionRequest {
  pub model: String,
  pub messages: Vec<Message>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub tools: Vec<ToolDefinition>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tool_choice: Option<ToolChoice>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub temperature: Option<f64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub max_tokens: Option<u64>,
  #[serde(flatten)]
  pub additional_params: Option<serde_json::Value>,
}

impl CompletionRequest {
  /// 从内部消息历史构造 wire 请求（preamble → system 消息打头）。
  pub fn from_history(
    model: impl Into<String>,
    preamble: Option<String>,
    history: Vec<core::Message>,
    tools: Vec<core::ToolDefinition>,
    tool_choice: Option<core::ToolChoice>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    additional_params: Option<serde_json::Value>,
  ) -> Result<Self, OpenAiCompatError> {
    let mut full_history: Vec<Message> = preamble.map_or_else(Vec::new, |preamble| vec![Message::system(&preamble)]);

    full_history.extend(
      history
        .into_iter()
        .map(try_from_message_to_vec_input_item)
        .collect::<Result<Vec<Vec<Message>>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>(),
    );

    if full_history.is_empty() {
      return Err(OpenAiCompatError::request_build("Completion request contained no messages"));
    }

    let tool_choice = tool_choice.map(ToolChoice::try_from).transpose()?;

    Ok(Self {
      model: model.into(),
      messages: full_history,
      tools: tools.into_iter().map(ToolDefinition::from).collect::<Vec<_>>(),
      tool_choice,
      temperature,
      max_tokens,
      additional_params,
    })
  }
}

impl CompletionModel {
  /// 非流式 Chat Completions 调用。
  pub async fn completion(&self, request: CompletionRequest) -> Result<CompletionResponse, OpenAiCompatError> {
    let span = if tracing::Span::current().is_disabled() {
      info_span!(
          target: "fusion_ai::completions",
          "chat",
          gen_ai.operation.name = "chat",
          gen_ai.provider.name = "openai-compatible",
          gen_ai.request.model = %self.model,
          gen_ai.usage.output_tokens = tracing::field::Empty,
          gen_ai.usage.input_tokens = tracing::field::Empty,
      )
    } else {
      tracing::Span::current()
    };

    let body = serde_json::to_vec(&request).map_err(OpenAiCompatError::from)?;

    async move {
      let response = self.client.post_json("/chat/completions", body).send().await?;

      if !response.status().is_success() {
        return Err(Client::error_from_response(response).await);
      }

      let text = response.text().await.map_err(|e| OpenAiCompatError::Transport(e.to_string()))?;
      let parsed: ApiResponse<CompletionResponse> = serde_json::from_str(&text).map_err(OpenAiCompatError::from)?;

      match parsed {
        ApiResponse::Ok(response) => {
          let span = tracing::Span::current();
          if let Some(usage) = &response.usage {
            span.record("gen_ai.usage.input_tokens", usage.prompt_tokens);
            span.record("gen_ai.usage.output_tokens", usage.total_tokens.saturating_sub(usage.prompt_tokens));
          }
          tracing::debug!("OpenAI response: {response:?}");
          Ok(response)
        }
        ApiResponse::Err(err) => Err(err.into()),
      }
    }
    .instrument(span)
    .await
  }
}
