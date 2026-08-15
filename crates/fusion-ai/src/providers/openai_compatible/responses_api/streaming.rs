//! Responses API 流式（eventsource-stream 解析）。
//!
//! 终止形态：Responses 流没有 `[DONE]` 哨兵，终态由
//! `response.completed` / `response.incomplete` / `response.failed` 事件携带——
//! 收到任一终态事件即结束流（DeepSeek Responses 无状态子集实测如此）。

use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::info_span;
use tracing_futures::Instrument as _;

use crate::providers::openai_compatible::completion::streaming::ToolCallDeltaContent;
use crate::providers::openai_compatible::errors::OpenAiCompatError;
use crate::providers::openai_compatible::responses_api::{CompletionRequest, ReasoningSummary, ResponsesCompletionModel};
use crate::providers::openai_compatible::Client;

use super::{CompletionResponse, Output};

// ================================================================
// OpenAI Responses Streaming API
// ================================================================

/// 流式终态：`response.completed` 携带的 usage。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StreamingCompletionResponse {
  /// Token usage
  pub usage: super::ResponsesUsage,
}

/// Responses 流事件（统一流事件枚举，Final 携带终态）。
#[derive(Debug, Clone)]
pub enum StreamingChoice {
  Text(String),
  ToolCall {
    id: String,
    call_id: Option<String>,
    name: String,
    arguments: serde_json::Value,
  },
  ToolCallDelta {
    id: String,
    content: ToolCallDeltaContent,
  },
  Reasoning {
    id: Option<String>,
    content: String,
  },
  Final(StreamingCompletionResponse),
}

/// Responses 流（`Stream<Item = Result<StreamingChoice, OpenAiCompatError>>`）。
pub struct ResponsesStream {
  inner: Pin<Box<dyn Stream<Item = Result<StreamingChoice, OpenAiCompatError>> + Send>>,
}

impl Stream for ResponsesStream {
  type Item = Result<StreamingChoice, OpenAiCompatError>;

  fn poll_next(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Option<Self::Item>> {
    Pin::new(&mut self.get_mut().inner).poll_next(cx)
  }
}

/// A streaming completion chunk.
/// Streaming chunks can come in one of two forms:
/// - A response chunk (where the completed response will have the total token usage)
/// - An item chunk commonly referred to as a delta. In the completions API this would be referred to as the message delta.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum StreamingCompletionChunk {
  Response(Box<ResponseChunk>),
  Delta(ItemChunk),
}

/// A response chunk from OpenAI's response API.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseChunk {
  /// The response chunk type
  #[serde(rename = "type")]
  pub kind: ResponseChunkKind,
  /// The response itself
  pub response: CompletionResponse,
  /// The item sequence
  pub sequence_number: u64,
}

/// Response chunk type.
/// Renames are used to ensure that this type gets (de)serialized properly.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum ResponseChunkKind {
  #[serde(rename = "response.created")]
  ResponseCreated,
  #[serde(rename = "response.in_progress")]
  ResponseInProgress,
  #[serde(rename = "response.completed")]
  ResponseCompleted,
  #[serde(rename = "response.failed")]
  ResponseFailed,
  #[serde(rename = "response.incomplete")]
  ResponseIncomplete,
}

/// An item message chunk from OpenAI's Responses API.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ItemChunk {
  /// Item ID. Optional.
  pub item_id: Option<String>,
  /// The output index of the item from a given streamed response.
  pub output_index: u64,
  /// The item type chunk, as well as the inner data.
  #[serde(flatten)]
  pub data: ItemChunkKind,
}

/// The item chunk type from OpenAI's Responses API.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ItemChunkKind {
  #[serde(rename = "response.output_item.added")]
  OutputItemAdded(StreamingItemDoneOutput),
  #[serde(rename = "response.output_item.done")]
  OutputItemDone(StreamingItemDoneOutput),
  #[serde(rename = "response.content_part.added")]
  ContentPartAdded(ContentPartChunk),
  #[serde(rename = "response.content_part.done")]
  ContentPartDone(ContentPartChunk),
  #[serde(rename = "response.output_text.delta")]
  OutputTextDelta(DeltaTextChunk),
  #[serde(rename = "response.output_text.done")]
  OutputTextDone(OutputTextChunk),
  #[serde(rename = "response.refusal.delta")]
  RefusalDelta(DeltaTextChunk),
  #[serde(rename = "response.refusal.done")]
  RefusalDone(RefusalTextChunk),
  #[serde(rename = "response.function_call_arguments.delta")]
  FunctionCallArgsDelta(DeltaTextChunkWithItemId),
  #[serde(rename = "response.function_call_arguments.done")]
  FunctionCallArgsDone(ArgsTextChunk),
  #[serde(rename = "response.reasoning_summary_part.added")]
  ReasoningSummaryPartAdded(SummaryPartChunk),
  #[serde(rename = "response.reasoning_summary_part.done")]
  ReasoningSummaryPartDone(SummaryPartChunk),
  #[serde(rename = "response.reasoning_summary_text.added")]
  ReasoningSummaryTextAdded(SummaryTextChunk),
  #[serde(rename = "response.reasoning_summary_text.done")]
  ReasoningSummaryTextDone(SummaryTextChunk),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StreamingItemDoneOutput {
  pub sequence_number: u64,
  pub item: Output,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContentPartChunk {
  pub content_index: u64,
  pub sequence_number: u64,
  pub part: ContentPartChunkPart,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ContentPartChunkPart {
  OutputText { text: String },
  SummaryText { text: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeltaTextChunk {
  pub content_index: u64,
  pub sequence_number: u64,
  pub delta: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeltaTextChunkWithItemId {
  pub item_id: String,
  pub content_index: u64,
  pub sequence_number: u64,
  pub delta: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OutputTextChunk {
  pub content_index: u64,
  pub sequence_number: u64,
  pub text: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RefusalTextChunk {
  pub content_index: u64,
  pub sequence_number: u64,
  pub refusal: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArgsTextChunk {
  pub content_index: u64,
  pub sequence_number: u64,
  pub arguments: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SummaryPartChunk {
  pub summary_index: u64,
  pub sequence_number: u64,
  pub part: SummaryPartChunkPart,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SummaryTextChunk {
  pub summary_index: u64,
  pub sequence_number: u64,
  pub delta: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum SummaryPartChunkPart {
  SummaryText { text: String },
}

impl ResponsesCompletionModel {
  /// 流式 Responses 调用（`stream` 参数由实现注入）。
  pub async fn stream(
    &self,
    request: CompletionRequest,
  ) -> Result<ResponsesStream, OpenAiCompatError> {
    stream_responses(&self.client, request).await
  }
}

pub(crate) async fn stream_responses(
  client: &Client,
  mut request: CompletionRequest,
) -> Result<ResponsesStream, OpenAiCompatError> {
  request.stream = Some(true);
  let model = request.model.clone();

  let body = serde_json::to_vec(&request).map_err(OpenAiCompatError::from)?;

  let response = client.post_json("/responses", body).send().await?;
  if !response.status().is_success() {
    return Err(Client::error_from_response(response).await);
  }

  let event_source = response.bytes_stream().eventsource();

  let span = info_span!(
      target: "fusion_ai::completions",
      "chat_streaming",
      gen_ai.operation.name = "chat_streaming",
      gen_ai.provider.name = "openai-compatible",
      gen_ai.request.model = %model,
  );
  let s = stream! {
      let span = tracing::Span::current();
      let mut final_usage = super::ResponsesUsage::new();
      let mut terminated = false;

      let mut tool_calls: Vec<StreamingChoice> = Vec::new();
      let mut combined_text = String::new();

      let mut event_source = std::pin::pin!(event_source);

      while !terminated {
          let event_result = event_source.next().await;
          let Some(event_result) = event_result else { break };
          let message = match event_result {
              Ok(message) => message,
              Err(error) => {
                  tracing::error!(?error, "SSE error");
                  yield Err(OpenAiCompatError::Stream(error.to_string()));
                  break;
              }
          };

          // Skip heartbeat messages or empty data
          if message.data.trim().is_empty() {
              continue;
          }

          let data = serde_json::from_str::<StreamingCompletionChunk>(&message.data);
          let Ok(data) = data else {
              let err = data.unwrap_err();
              tracing::debug!("Couldn't serialize data as StreamingCompletionResponse: {:?}", err);
              continue;
          };

          if let StreamingCompletionChunk::Delta(chunk) = &data {
              match &chunk.data {
                  ItemChunkKind::OutputItemDone(message) => {
                      match message {
                          StreamingItemDoneOutput { item: Output::FunctionCall(func), .. } => {
                              tool_calls.push(StreamingChoice::ToolCall {
                                  id: func.id.clone(),
                                  call_id: Some(func.call_id.clone()),
                                  name: func.name.clone(),
                                  arguments: func.arguments.clone(),
                              });
                          }

                          StreamingItemDoneOutput { item: Output::Reasoning { summary, id }, .. } => {
                              let reasoning = summary
                                  .iter()
                                  .map(|x| {
                                      let ReasoningSummary::SummaryText { text } = x;
                                      text.to_owned()
                                  })
                                  .collect::<Vec<String>>()
                                  .join("\n");
                              yield Ok(StreamingChoice::Reasoning { content: reasoning, id: Some(id.to_string()) })
                          }
                          _ => continue
                      }
                  }
                  ItemChunkKind::OutputTextDelta(delta) => {
                      combined_text.push_str(&delta.delta);
                      yield Ok(StreamingChoice::Text(delta.delta.clone()))
                  }
                  ItemChunkKind::RefusalDelta(delta) => {
                      combined_text.push_str(&delta.delta);
                      yield Ok(StreamingChoice::Text(delta.delta.clone()))
                  }
                  ItemChunkKind::FunctionCallArgsDelta(delta) => {
                      yield Ok(StreamingChoice::ToolCallDelta {
                          id: delta.item_id.clone(),
                          content: ToolCallDeltaContent::Delta(delta.delta.clone()),
                      })
                  }

                  _ => { continue }
              }
          }

          if let StreamingCompletionChunk::Response(chunk) = data {
              match chunk.kind {
                  ResponseChunkKind::ResponseCompleted => {
                      let response = chunk.response;
                      span.record("gen_ai.output.messages", serde_json::to_string(&response.output).unwrap_or_default());
                      span.record("gen_ai.response.id", response.id);
                      span.record("gen_ai.response.model", response.model);
                      if let Some(usage) = response.usage {
                          final_usage = usage;
                      }
                      // 终止形态二：Responses 流没有 [DONE]，终态事件即结束
                      terminated = true;
                  }
                  ResponseChunkKind::ResponseFailed | ResponseChunkKind::ResponseIncomplete => {
                      // provider 侧终态（失败 / 截断）：结束流，usage 保持已聚合值
                      terminated = true;
                  }
                  _ => {}
              }
          }
      }

      for tool_call in &tool_calls {
          yield Ok(tool_call.to_owned())
      }

      span.record("gen_ai.usage.input_tokens", final_usage.input_tokens);
      span.record("gen_ai.usage.output_tokens", final_usage.output_tokens);
      tracing::info!("Responses stream finished");

      yield Ok(StreamingChoice::Final(StreamingCompletionResponse {
          usage: final_usage.clone()
      }));
  }
  .instrument(span);

  Ok(ResponsesStream { inner: Box::pin(s) })
}
