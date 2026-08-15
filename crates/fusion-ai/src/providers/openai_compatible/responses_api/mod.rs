//! The OpenAI Responses API（类型本地化）。
//!
//! `Client::completion_model()` 默认返回本模块的 Responses 模型；
//! `.completions_api()` 显式切回 Chat Completions。
//! ```rust
//! use fusion_ai::providers::openai_compatible::Client;
//!
//! let openai_client = Client::new("YOUR_API_KEY");
//! let model = openai_client.completion_model("gpt-4o").completions_api();
//! ```
use std::convert::Infallible;
use std::ops::Add;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{Instrument, info_span};

use crate::json_utils;
use crate::providers::openai_compatible::errors::OpenAiCompatError;
use crate::providers::openai_compatible::types as core;
use crate::providers::openai_compatible::types::{DocumentSourceKind, DocumentMediaType, OneOrMany};

use super::completion::ToolChoice;
use super::types::{ImageDetail, Text};
use super::{Client, InputAudio, SystemContent};

pub mod streaming;

/// The completion request type for OpenAI's Response API: <https://platform.openai.com/docs/api-reference/responses/create>
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CompletionRequest {
  /// Message inputs
  pub input: OneOrMany<InputItem>,
  /// The model name
  pub model: String,
  /// Instructions (also referred to as preamble, although in other APIs this would be the "system prompt")
  #[serde(skip_serializing_if = "Option::is_none")]
  pub instructions: Option<String>,
  /// The maximum number of output tokens.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub max_output_tokens: Option<u64>,
  /// Toggle to true for streaming responses.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub stream: Option<bool>,
  /// The temperature. Set higher (up to a max of 1.0) for more creative responses.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub temperature: Option<f64>,
  /// Whether the LLM should be forced to use a tool before returning a response.
  /// If none provided, the default option is "auto".
  #[serde(skip_serializing_if = "Option::is_none")]
  tool_choice: Option<ToolChoice>,
  /// The tools you want to use. Currently this is limited to functions, but will be expanded on in future.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub tools: Vec<ResponsesToolDefinition>,
  /// Additional parameters
  #[serde(flatten)]
  pub additional_parameters: AdditionalParameters,
}

impl CompletionRequest {
  pub fn with_structured_outputs<S>(mut self, schema_name: S, schema: serde_json::Value) -> Self
  where
    S: Into<String>,
  {
    self.additional_parameters.text = Some(TextConfig::structured_output(schema_name, schema));

    self
  }

  pub fn with_reasoning(mut self, reasoning: Reasoning) -> Self {
    self.additional_parameters.reasoning = Some(reasoning);

    self
  }
}

/// An input item for [`CompletionRequest`].
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct InputItem {
  /// The role of an input item/message.
  /// Input messages should be Some(Role::User), and output messages should be Some(Role::Assistant).
  /// Everything else should be None.
  #[serde(skip_serializing_if = "Option::is_none")]
  role: Option<Role>,
  /// The input content itself.
  #[serde(flatten)]
  input: InputContent,
}

/// Message roles. Used by OpenAI Responses API to determine who created a given message.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum Role {
  User,
  Assistant,
  System,
}

/// The type of content used in an [`InputItem`]. Additionally holds data for each type of input content.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
  Message(Message),
  Reasoning(OpenAIReasoning),
  FunctionCall(OutputFunctionCall),
  FunctionCallOutput(ToolResult),
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct OpenAIReasoning {
  id: String,
  pub summary: Vec<ReasoningSummary>,
  // OpenAI Responses 方言是 snake_case（encrypted_content），不得 camelCase 化
  pub encrypted_content: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub status: Option<ToolStatus>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSummary {
  SummaryText { text: String },
}

impl ReasoningSummary {
  fn new(input: &str) -> Self {
    Self::SummaryText { text: input.to_string() }
  }

  pub fn text(&self) -> String {
    let ReasoningSummary::SummaryText { text } = self;
    text.clone()
  }
}

/// A tool result.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ToolResult {
  /// The call ID of a tool (this should be linked to the call ID for a tool call, otherwise an error will be received)
  call_id: String,
  /// The result of a tool call.
  output: String,
  /// The status of a tool call (if used in a completion request, this should always be Completed)
  status: ToolStatus,
}

impl From<Message> for InputItem {
  fn from(value: Message) -> Self {
    match value {
      Message::User { .. } => Self { role: Some(Role::User), input: InputContent::Message(value) },
      Message::Assistant { ref content, .. } => {
        let role = if content.clone().iter().any(|x| matches!(x, AssistantContentType::Reasoning(_))) {
          None
        } else {
          Some(Role::Assistant)
        };
        Self { role, input: InputContent::Message(value) }
      }
      Message::System { .. } => Self { role: Some(Role::System), input: InputContent::Message(value) },
      Message::ToolResult { tool_call_id, output } => Self {
        role: None,
        input: InputContent::FunctionCallOutput(ToolResult {
          call_id: tool_call_id,
          output,
          status: ToolStatus::Completed,
        }),
      },
    }
  }
}

pub fn try_from_message_to_vec_input_item(value: core::Message) -> Result<Vec<InputItem>, OpenAiCompatError> {
  use crate::providers::openai_compatible::types as core;

  match value {
    core::Message::System { content } => Ok(vec![InputItem {
      role: Some(Role::System),
      input: InputContent::Message(Message::System {
        content: OneOrMany::one(SystemContent { r#type: Default::default(), text: content }),
        name: None,
      }),
    }]),
    core::Message::User { content } => {
      let mut items = Vec::new();

      for user_content in content {
        match user_content {
          core::UserContent::Text(Text { text, .. }) => {
            items.push(InputItem {
              role: Some(Role::User),
              input: InputContent::Message(Message::User {
                content: OneOrMany::one(UserContent::InputText { text }),
                name: None,
              }),
            });
          }
          core::UserContent::ToolResult(core::ToolResult {
            id,
            call_id,
            content: tool_content,
          }) => {
            let fallback_call_id = id;
            for tool_result_content in tool_content {
              let core::ToolResultContent::Text(Text { text, .. }) = tool_result_content else {
                return Err(OpenAiCompatError::request_build("Responses API only supports text tool results"));
              };
              let call_id = call_id.clone().unwrap_or_else(|| fallback_call_id.clone());
              items.push(InputItem {
                role: None,
                input: InputContent::FunctionCallOutput(ToolResult {
                  call_id,
                  output: text,
                  status: ToolStatus::Completed,
                }),
              });
            }
          }
          core::UserContent::Document(core::Document { data, media_type: Some(DocumentMediaType::PDF), .. }) => {
            let (file_data, file_url) = match data {
              DocumentSourceKind::Base64(data) => (Some(format!("data:application/pdf;base64,{data}")), None),
              DocumentSourceKind::Url(url) => (None, Some(url)),
              DocumentSourceKind::Raw(_) => {
                return Err(OpenAiCompatError::request_build("Raw file data not supported, encode as base64 first"));
              }
              doc => {
                return Err(OpenAiCompatError::request_build(format!("Unsupported document type: {doc}")));
              }
            };

            items.push(InputItem {
              role: Some(Role::User),
              input: InputContent::Message(Message::User {
                content: OneOrMany::one(UserContent::InputFile {
                  file_data,
                  file_url,
                  filename: Some("document.pdf".to_string()),
                }),
                name: None,
              }),
            })
          }
          // todo: should we ensure this takes into account file size?
          core::UserContent::Document(core::Document { data: DocumentSourceKind::Base64(text), .. }) => {
            items.push(InputItem {
              role: Some(Role::User),
              input: InputContent::Message(Message::User {
                content: OneOrMany::one(UserContent::InputText { text }),
                name: None,
              }),
            })
          }
          core::UserContent::Document(core::Document { data: DocumentSourceKind::String(text), .. }) => {
            items.push(InputItem {
              role: Some(Role::User),
              input: InputContent::Message(Message::User {
                content: OneOrMany::one(UserContent::InputText { text }),
                name: None,
              }),
            })
          }
          core::UserContent::Image(core::Image { data, media_type, detail, .. }) => {
            let url = match data {
              DocumentSourceKind::Base64(data) => {
                let media_type =
                  if let Some(media_type) = media_type { media_type.to_mime_type().to_string() } else { String::new() };
                format!("data:{media_type};base64,{data}")
              }
              DocumentSourceKind::Url(url) => url,
              DocumentSourceKind::Raw(_) => {
                return Err(OpenAiCompatError::request_build("Raw file data not supported, encode as base64 first"));
              }
              doc => {
                return Err(OpenAiCompatError::request_build(format!("Unsupported document type: {doc}")));
              }
            };
            items.push(InputItem {
              role: Some(Role::User),
              input: InputContent::Message(Message::User {
                content: OneOrMany::one(UserContent::InputImage { image_url: url, detail: detail.unwrap_or_default() }),
                name: None,
              }),
            });
          }
          message => {
            return Err(OpenAiCompatError::request_build(format!("Unsupported message: {message:?}")));
          }
        }
      }

      Ok(items)
    }
    core::Message::Assistant { id, content } => {
      let mut items = Vec::new();

      for assistant_content in content {
        match assistant_content {
          core::AssistantContent::Text(Text { text, .. }) => {
            let id = id.as_ref().unwrap_or(&String::default()).clone();
            items.push(InputItem {
              role: Some(Role::Assistant),
              input: InputContent::Message(Message::Assistant {
                content: OneOrMany::one(AssistantContentType::Text(AssistantContent::OutputText(Text::new(text)))),
                id,
                name: None,
                status: ToolStatus::Completed,
              }),
            });
          }
          core::AssistantContent::ToolCall(core::ToolCall {
            id: tool_id, call_id, function, ..
          }) => {
            items.push(InputItem {
              role: None,
              input: InputContent::FunctionCall(OutputFunctionCall {
                arguments: function.arguments,
                call_id: call_id.unwrap_or_default(),
                id: tool_id,
                name: function.name,
                status: ToolStatus::Completed,
              }),
            });
          }
          core::AssistantContent::Reasoning(core::Reasoning { id, content, .. }) => {
            let id = id.ok_or_else(|| {
              OpenAiCompatError::request_build(
                "An OpenAI-generated ID is required when using OpenAI reasoning items",
              )
            })?;
            items.push(InputItem {
              role: None,
              input: InputContent::Reasoning(OpenAIReasoning {
                id,
                summary: content
                  .into_iter()
                  .map(|x| {
                    let text = match x {
                      core::ReasoningContent::Text { text, .. } => text.clone(),
                      core::ReasoningContent::Encrypted(s) => s.clone(),
                      core::ReasoningContent::Redacted { data } => data.clone(),
                      core::ReasoningContent::Summary(s) => s.clone(),
                    };
                    ReasoningSummary::new(&text)
                  })
                  .collect(),
                encrypted_content: None,
                status: None,
              }),
            });
          }
          core::AssistantContent::Image(_) => {
            return Err(OpenAiCompatError::request_build(
              "OpenAI Responses API does not support image content in assistant messages",
            ));
          }
        }
      }

      Ok(items)
    }
  }
}

/// The definition of a tool response, repurposed for OpenAI's Responses API.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ResponsesToolDefinition {
  /// Tool name
  pub name: String,
  /// Parameters - this should be a JSON schema. Tools should additionally ensure an "additionalParameters" field has been added with the value set to false, as this is required if using OpenAI's strict mode (enabled by default).
  pub parameters: serde_json::Value,
  /// Whether to use strict mode. Enabled by default as it allows for improved efficiency.
  pub strict: bool,
  /// The type of tool. This should always be "function".
  #[serde(rename = "type")]
  pub kind: String,
  /// Tool description.
  pub description: String,
}

/// Recursively ensures all object schemas in a JSON schema have `additionalProperties: false`.
/// Nested arrays, schema $defs, object properties and enums should be handled through this method
/// This seems to be required by OpenAI's Responses API when using strict mode.
fn add_props_false(schema: &mut serde_json::Value) {
  if let Value::Object(obj) = schema {
    let is_object_schema =
      obj.get("type") == Some(&Value::String("object".to_string())) || obj.contains_key("properties");

    if is_object_schema && !obj.contains_key("additionalProperties") {
      obj.insert("additionalProperties".to_string(), Value::Bool(false));
    }

    if let Some(defs) = obj.get_mut("$defs")
      && let Value::Object(defs_obj) = defs
    {
      for (_, def_schema) in defs_obj.iter_mut() {
        add_props_false(def_schema);
      }
    }

    if let Some(properties) = obj.get_mut("properties")
      && let Value::Object(props) = properties
    {
      for (_, prop_value) in props.iter_mut() {
        add_props_false(prop_value);
      }
    }

    if let Some(items) = obj.get_mut("items") {
      add_props_false(items);
    }

    // should handle Enums (anyOf/oneOf)
    for key in ["anyOf", "oneOf", "allOf"] {
      if let Some(variants) = obj.get_mut(key)
        && let Value::Array(variants_array) = variants
      {
        for variant in variants_array.iter_mut() {
          add_props_false(variant);
        }
      }
    }
  }
}

impl From<core::ToolDefinition> for ResponsesToolDefinition {
  fn from(value: core::ToolDefinition) -> Self {
    let core::ToolDefinition { name, mut parameters, description } = value;

    add_props_false(&mut parameters);

    Self { name, parameters, description, kind: "function".to_string(), strict: true }
  }
}

/// Token usage.
/// Token usage from the OpenAI Responses API generally shows the input tokens and output tokens (both with more in-depth details) as well as a total tokens field.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponsesUsage {
  /// Input tokens
  pub input_tokens: u64,
  /// In-depth detail on input tokens (cached tokens)
  #[serde(skip_serializing_if = "Option::is_none")]
  pub input_tokens_details: Option<InputTokensDetails>,
  /// Output tokens
  pub output_tokens: u64,
  /// In-depth detail on output tokens (reasoning tokens)
  pub output_tokens_details: OutputTokensDetails,
  /// Total tokens used (for a given prompt)
  pub total_tokens: u64,
}

impl ResponsesUsage {
  /// Create a new ResponsesUsage instance
  pub(crate) fn new() -> Self {
    Self {
      input_tokens: 0,
      input_tokens_details: Some(InputTokensDetails::new()),
      output_tokens: 0,
      output_tokens_details: OutputTokensDetails::new(),
      total_tokens: 0,
    }
  }
}

impl Add for ResponsesUsage {
  type Output = Self;

  fn add(self, rhs: Self) -> Self::Output {
    let input_tokens = self.input_tokens + rhs.input_tokens;
    let input_tokens_details = self
      .input_tokens_details
      .map(|lhs| if let Some(tokens) = rhs.input_tokens_details { lhs + tokens } else { lhs });
    let output_tokens = self.output_tokens + rhs.output_tokens;
    let output_tokens_details = self.output_tokens_details + rhs.output_tokens_details;
    let total_tokens = self.total_tokens + rhs.total_tokens;
    Self { input_tokens, input_tokens_details, output_tokens, output_tokens_details, total_tokens }
  }
}

/// In-depth details on input tokens.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputTokensDetails {
  /// Cached tokens from OpenAI
  pub cached_tokens: u64,
}

impl InputTokensDetails {
  pub(crate) fn new() -> Self {
    Self { cached_tokens: 0 }
  }
}

impl Add for InputTokensDetails {
  type Output = Self;
  fn add(self, rhs: Self) -> Self::Output {
    Self { cached_tokens: self.cached_tokens + rhs.cached_tokens }
  }
}

/// In-depth details on output tokens.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputTokensDetails {
  /// Reasoning tokens
  pub reasoning_tokens: u64,
}

impl OutputTokensDetails {
  pub(crate) fn new() -> Self {
    Self { reasoning_tokens: 0 }
  }
}

impl Add for OutputTokensDetails {
  type Output = Self;
  fn add(self, rhs: Self) -> Self::Output {
    Self { reasoning_tokens: self.reasoning_tokens + rhs.reasoning_tokens }
  }
}

/// Occasionally, when using OpenAI's Responses API you may get an incomplete response. This struct holds the reason as to why it happened.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IncompleteDetailsReason {
  /// The reason for an incomplete [`CompletionResponse`].
  pub reason: String,
}

/// A response error from OpenAI's Response API.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResponseError {
  /// Error code
  pub code: String,
  /// Error message
  pub message: String,
}

/// A response object as an enum (ensures type validation)
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseObject {
  Response,
}

/// The response status as an enum (ensures type validation)
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
  InProgress,
  Completed,
  Failed,
  Cancelled,
  Queued,
  Incomplete,
}

impl CompletionRequest {
  /// 从内部消息历史构造 Responses wire 请求（preamble → `instructions`）。
  ///
  /// `additional_params` 是 extra-body 注入点（reasoning 控制等 provider 专属参数）。
  #[allow(clippy::too_many_arguments)]
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
    let full_history: Vec<InputItem> = history
      .into_iter()
      .map(try_from_message_to_vec_input_item)
      .collect::<Result<Vec<Vec<InputItem>>, _>>()?
      .into_iter()
      .flatten()
      .collect::<Vec<InputItem>>();

    let input = OneOrMany::many(full_history)
      .map_err(|_| OpenAiCompatError::request_build("Completion request contained no input messages"))?;

    let stream = additional_params.clone().unwrap_or(Value::Null).as_bool();

    let additional_parameters = if let Some(map) = additional_params {
      serde_json::from_value::<AdditionalParameters>(map)?
    } else {
      // If there's no additional parameters, initialise an empty object
      AdditionalParameters::default()
    };

    let tool_choice = tool_choice.map(ToolChoice::try_from).transpose()?;

    Ok(Self {
      input,
      model: model.into(),
      instructions: preamble,
      max_output_tokens: max_tokens,
      stream,
      tool_choice,
      tools: tools.into_iter().map(ResponsesToolDefinition::from).collect(),
      temperature,
      additional_parameters,
    })
  }
}

/// The completion model struct for OpenAI's response API.
#[derive(Clone)]
pub struct ResponsesCompletionModel {
  /// The OpenAI client
  pub(crate) client: Client,
  /// Name of the model (e.g.: gpt-3.5-turbo-1106)
  pub model: String,
}

impl ResponsesCompletionModel {
  /// Creates a new [`ResponsesCompletionModel`].
  pub fn new(client: Client, model: &str) -> Self {
    Self { client, model: model.to_string() }
  }

  /// Use the Completions API instead of Responses.
  pub fn completions_api(self) -> crate::providers::openai_compatible::completion::CompletionModel {
    crate::providers::openai_compatible::completion::CompletionModel::new(self.client, &self.model)
  }
}

/// The standard response format from OpenAI's Responses API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletionResponse {
  /// The ID of a completion response.
  pub id: String,
  /// The type of the object.
  pub object: ResponseObject,
  /// The time at which a given response has been created, in seconds from the UNIX epoch (01/01/1970 00:00:00).
  pub created_at: u64,
  /// The status of the response.
  pub status: ResponseStatus,
  /// Response error (optional)
  pub error: Option<ResponseError>,
  /// Incomplete response details (optional)
  pub incomplete_details: Option<IncompleteDetailsReason>,
  /// System prompt/preamble
  pub instructions: Option<String>,
  /// The maximum number of tokens the model should output
  pub max_output_tokens: Option<u64>,
  /// The model name
  pub model: String,
  /// Token usage
  pub usage: Option<ResponsesUsage>,
  /// The model output (messages, etc will go here)
  pub output: Vec<Output>,
  /// Tools
  #[serde(default)]
  pub tools: Vec<ResponsesToolDefinition>,
  /// Additional parameters
  #[serde(flatten)]
  pub additional_parameters: AdditionalParameters,
}

/// Additional parameters for the completion request type for OpenAI's Response API: <https://platform.openai.com/docs/api-reference/responses/create>
/// 由 [`CompletionRequest::from_history`] 从内部消息历史构造。
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct AdditionalParameters {
  /// Whether or not a given model task should run in the background (ie a detached process).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub background: Option<bool>,
  /// The text response format. This is where you would add structured outputs (if you want them).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub text: Option<TextConfig>,
  /// What types of extra data you would like to include. This is mostly useless at the moment since the types of extra data to add is currently unsupported, but this will be coming soon!
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include: Option<Vec<Include>>,
  /// `top_p`. Mutually exclusive with the `temperature` argument.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub top_p: Option<f64>,
  /// Whether or not the response should be truncated.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub truncation: Option<TruncationStrategy>,
  /// The username of the user (that you want to use).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub user: Option<String>,
  /// Any additional metadata you'd like to add. This will additionally be returned by the response.
  #[serde(skip_serializing_if = "Map::is_empty", default)]
  pub metadata: serde_json::Map<String, serde_json::Value>,
  /// Whether or not you want tool calls to run in parallel.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub parallel_tool_calls: Option<bool>,
  /// Previous response ID. If you are not sending a full conversation, this can help to track the message flow.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub previous_response_id: Option<String>,
  /// Add thinking/reasoning to your response. The response will be emitted as a list member of the `output` field.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reasoning: Option<Reasoning>,
  /// The service tier you're using.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub service_tier: Option<OpenAIServiceTier>,
  /// Whether or not to store the response for later retrieval by API.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub store: Option<bool>,
}

impl AdditionalParameters {
  pub fn to_json(self) -> serde_json::Value {
    // Serializing a plain struct of `Option`/scalar fields is infallible in
    // practice; on the unreachable failure path fall back to `Value::Null`.
    serde_json::to_value(self).unwrap_or_default()
  }
}

/// The truncation strategy.
/// When using auto, if the context of this response and previous ones exceeds the model's context window size, the model will truncate the response to fit the context window by dropping input items in the middle of the conversation.
/// Otherwise, does nothing (and is disabled by default).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationStrategy {
  Auto,
  #[default]
  Disabled,
}

/// The model output format configuration.
/// You can either have plain text by default, or attach a JSON schema for the purposes of structured outputs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextConfig {
  pub format: TextFormat,
}

impl TextConfig {
  pub(crate) fn structured_output<S>(name: S, schema: serde_json::Value) -> Self
  where
    S: Into<String>,
  {
    Self { format: TextFormat::JsonSchema(StructuredOutputsInput { name: name.into(), schema, strict: true }) }
  }
}

/// The text format (contained by [`TextConfig`]).
/// You can either have plain text by default, or attach a JSON schema for the purposes of structured outputs.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
  JsonSchema(StructuredOutputsInput),
  #[default]
  Text,
}

/// The inputs required for adding structured outputs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StructuredOutputsInput {
  /// The name of your schema.
  pub name: String,
  /// Your required output schema. It is recommended that you use the JsonSchema macro, which you can check out at <https://docs.rs/schemars/latest/schemars/trait.JsonSchema.html>.
  pub schema: serde_json::Value,
  /// Enable strict output. If you are using your AI agent in a data pipeline or another scenario that requires the data to be absolutely fixed to a given schema, it is recommended to set this to true.
  pub strict: bool,
}

/// Add reasoning to a [`CompletionRequest`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Reasoning {
  /// How much effort you want the model to put into thinking/reasoning.
  pub effort: Option<ReasoningEffort>,
  /// How much effort you want the model to put into writing the reasoning summary.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub summary: Option<ReasoningSummaryLevel>,
}

impl Reasoning {
  /// Creates a new Reasoning instantiation (with empty values).
  pub fn new() -> Self {
    Self { effort: None, summary: None }
  }

  /// Adds reasoning effort.
  pub fn with_effort(mut self, reasoning_effort: ReasoningEffort) -> Self {
    self.effort = Some(reasoning_effort);

    self
  }

  /// Adds summary level (how detailed the reasoning summary will be).
  pub fn with_summary_level(mut self, reasoning_summary_level: ReasoningSummaryLevel) -> Self {
    self.summary = Some(reasoning_summary_level);

    self
  }
}

/// The billing service tier that will be used. On auto by default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAIServiceTier {
  #[default]
  Auto,
  Default,
  Flex,
}

/// The amount of reasoning effort that will be used by a given model.
///
/// `None` = 关闭思考（DashScope Responses 方言：reasoning.effort 优先于 enable_thinking）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
  None,
  Minimal,
  Low,
  #[default]
  Medium,
  High,
}

/// The amount of effort that will go into a reasoning summary by a given model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryLevel {
  #[default]
  Auto,
  Concise,
  Detailed,
}

/// Results to additionally include in the OpenAI Responses API.
/// Note that most of these are currently unsupported, but have been added for completeness.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Include {
  #[serde(rename = "file_search_call.results")]
  FileSearchCallResults,
  #[serde(rename = "message.input_image.image_url")]
  MessageInputImageImageUrl,
  #[serde(rename = "computer_call.output.image_url")]
  ComputerCallOutputOutputImageUrl,
  #[serde(rename = "reasoning.encrypted_content")]
  ReasoningEncryptedContent,
  #[serde(rename = "code_interpreter_call.outputs")]
  CodeInterpreterCallOutputs,
}

/// A currently non-exhaustive list of output types.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Output {
  Message(OutputMessage),
  #[serde(alias = "function_call")]
  FunctionCall(OutputFunctionCall),
  Reasoning {
    id: String,
    summary: Vec<ReasoningSummary>,
  },
}

impl From<Output> for Vec<core::AssistantContent> {
  fn from(value: Output) -> Self {
    match value {
      Output::Message(OutputMessage { content, .. }) => {
        content.into_iter().map(core::AssistantContent::from).collect()
      }
      Output::FunctionCall(OutputFunctionCall { id, arguments, call_id, name, .. }) => {
        vec![core::AssistantContent::ToolCall(
          core::ToolCall::new(id, core::ToolFunction { name, arguments }).with_call_id(call_id),
        )]
      }
      Output::Reasoning { id, summary } => {
        let summary: Vec<String> = summary.into_iter().map(|x| x.text()).collect();

        vec![core::AssistantContent::Reasoning(
          core::Reasoning {
            id: Some(id),
            content: vec![core::ReasoningContent::Summary(summary.join("\n"))],
          },
        )]
      }
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OutputReasoning {
  id: String,
  summary: Vec<ReasoningSummary>,
  status: ToolStatus,
}

/// An OpenAI Responses API tool call. A call ID will be returned that must be used when creating a tool result to send back to OpenAI as a message input, otherwise an error will be received.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OutputFunctionCall {
  pub id: String,
  #[serde(with = "json_utils::stringified_json")]
  pub arguments: serde_json::Value,
  pub call_id: String,
  pub name: String,
  pub status: ToolStatus,
}

/// The status of a given tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
  InProgress,
  Completed,
  Incomplete,
}

/// An output message from OpenAI's Responses API.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OutputMessage {
  /// The message ID. Must be included when sending the message back to OpenAI
  pub id: String,
  /// The role (currently only Assistant is available as this struct is only created when receiving an LLM message as a response)
  pub role: OutputRole,
  /// The status of the response
  pub status: ResponseStatus,
  /// The actual message content
  pub content: Vec<AssistantContent>,
}

/// The role of an output message.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OutputRole {
  Assistant,
}

impl ResponsesCompletionModel {
  /// 非流式 Responses 调用。
  pub async fn completion(&self, request: CompletionRequest) -> Result<CompletionResponse, OpenAiCompatError> {
    let span = if tracing::Span::current().is_disabled() {
      info_span!(
          target: "fusion_ai::completions",
          "chat",
          gen_ai.operation.name = "chat",
          gen_ai.provider.name = "openai-compatible",
          gen_ai.request.model = tracing::field::Empty,
          gen_ai.response.id = tracing::field::Empty,
          gen_ai.response.model = tracing::field::Empty,
          gen_ai.usage.output_tokens = tracing::field::Empty,
          gen_ai.usage.input_tokens = tracing::field::Empty,
          gen_ai.input.messages = tracing::field::Empty,
          gen_ai.output.messages = tracing::field::Empty,
      )
    } else {
      tracing::Span::current()
    };

    span.record("gen_ai.request.model", &self.model);
    span.record(
      "gen_ai.input.messages",
      // Tracing attribute only — degrade to empty string rather than panic.
      serde_json::to_string(&request.input).unwrap_or_default(),
    );
    let body = serde_json::to_vec(&request).map_err(OpenAiCompatError::from)?;
    tracing::debug!("OpenAI Responses API input: {request}", request = serde_json::to_string_pretty(&request).unwrap());

    async move {
      let response = self.client.post_json("/responses", body).send().await?;

      if response.status().is_success() {
        let t = response.text().await.map_err(|e| OpenAiCompatError::Transport(e.to_string()))?;
        let response = serde_json::from_str::<CompletionResponse>(&t).map_err(OpenAiCompatError::from)?;
        let span = tracing::Span::current();
        span.record("gen_ai.output.messages", serde_json::to_string(&response.output).unwrap_or_default());
        span.record("gen_ai.response.id", &response.id);
        span.record("gen_ai.response.model", &response.model);
        if let Some(ref usage) = response.usage {
          span.record("gen_ai.usage.output_tokens", usage.output_tokens);
          span.record("gen_ai.usage.input_tokens", usage.input_tokens);
        }
        tracing::info!("API successfully called");
        Ok(response)
      } else {
        Err(Client::error_from_response(response).await)
      }
    }
    .instrument(span)
    .await
  }

}

impl CompletionResponse {
  /// 提取全部 output 为内部消息内容（message 文本 / 工具调用 / reasoning）。
  pub fn assistant_content(&self) -> Vec<core::AssistantContent> {
    self.output.iter().cloned().flat_map(<Vec<core::AssistantContent>>::from).collect()
  }

  /// 首段 message output 的拼接文本。
  pub fn text(&self) -> Option<String> {
    let text = self
      .output
      .iter()
      .filter_map(|output| match output {
        Output::Message(message) => Some(
          message
            .content
            .iter()
            .map(|c| match c {
              AssistantContent::OutputText(Text { text, .. }) => text.as_str(),
              AssistantContent::Refusal { refusal } => refusal.as_str(),
            })
            .collect::<Vec<_>>()
            .join(""),
        ),
        _ => None,
      })
      .collect::<Vec<_>>()
      .join("");
    Some(text).filter(|s| !s.is_empty())
  }

  /// 工具调用列表。
  pub fn tool_calls(&self) -> Vec<&OutputFunctionCall> {
    self
      .output
      .iter()
      .filter_map(|output| match output {
        Output::FunctionCall(call) => Some(call),
        _ => None,
      })
      .collect()
  }

  /// 通用 token 用量（provider 无关形态）。
  pub fn usage_tokens(&self) -> core::Usage {
    match &self.usage {
      Some(usage) => core::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
      },
      None => core::Usage::default(),
    }
  }
}

/// An OpenAI Responses API message.
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
    content: OneOrMany<AssistantContentType>,
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    status: ToolStatus,
  },
  #[serde(rename = "tool")]
  ToolResult { tool_call_id: String, output: String },
}

/// The type of a tool result content item.
#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultContentType {
  #[default]
  Text,
}

impl Message {
  pub fn system(content: &str) -> Self {
    Message::System { content: OneOrMany::one(content.to_owned().into()), name: None }
  }
}

/// Text assistant content.
/// Note that the text type in comparison to the Completions API is actually `output_text` rather than `text`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
  OutputText(Text),
  Refusal { refusal: String },
}

impl From<AssistantContent> for core::AssistantContent {
  fn from(value: AssistantContent) -> Self {
    match value {
      AssistantContent::Refusal { refusal } => core::AssistantContent::Text(core::Text::new(refusal)),
      AssistantContent::OutputText(Text { text, .. }) => core::AssistantContent::Text(core::Text::new(text)),
    }
  }
}

/// The type of assistant content.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(untagged)]
pub enum AssistantContentType {
  Text(AssistantContent),
  ToolCall(OutputFunctionCall),
  Reasoning(OpenAIReasoning),
}

/// Different types of user content.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
  InputText {
    text: String,
  },
  InputImage {
    image_url: String,
    #[serde(default)]
    detail: ImageDetail,
  },
  InputFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    file_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
  },
  Audio {
    input_audio: InputAudio,
  },
  #[serde(rename = "tool")]
  ToolResult {
    tool_call_id: String,
    output: String,
  },
}

impl FromStr for UserContent {
  type Err = Infallible;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(UserContent::InputText { text: s.to_string() })
  }
}
