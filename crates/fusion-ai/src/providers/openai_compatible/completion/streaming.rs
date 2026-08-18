//! Chat Completions 流式（eventsource-stream 解析，`data: [DONE]` 终止形态）。

use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use tracing::info_span;
use tracing_futures::Instrument as _;

use crate::json_utils::{self, merge};
use crate::providers::openai_compatible::Client;
use crate::providers::openai_compatible::completion::{CompletionModel, CompletionRequest, Usage};
use crate::providers::openai_compatible::errors::OpenAiCompatError;
use crate::providers::openai_compatible::types as core;

// ================================================================
// OpenAI Completion Streaming API
// ================================================================

/// 流式终态：聚合 usage（final chunk 或 `stream_options.include_usage` 提供）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingCompletionResponse {
  pub usage: Usage,
}

impl StreamingCompletionResponse {
  /// 通用 token 用量（provider 无关形态；cache 命中双方言解析见 wire `Usage`）。
  pub fn usage_tokens(&self) -> core::Usage {
    core::Usage {
      input_tokens: self.usage.prompt_tokens as u64,
      output_tokens: self.usage.total_tokens.saturating_sub(self.usage.prompt_tokens) as u64,
      total_tokens: self.usage.total_tokens as u64,
      cached_input_tokens: self.usage.cached_input_tokens,
      cache_creation_input_tokens: 0,
    }
  }
}

/// 流式工具调用参数分片内容。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallDeltaContent {
  /// provider 流出的函数名
  Name(String),
  /// 参数 JSON 分片
  Delta(String),
}

/// Chat Completions 流事件（统一流事件枚举，Final 携带终态）。
#[derive(Debug, Clone)]
pub enum StreamingChoice {
  Text(String),
  ToolCall { id: String, call_id: Option<String>, name: String, arguments: serde_json::Value },
  ToolCallDelta { id: String, content: ToolCallDeltaContent },
  Final(StreamingCompletionResponse),
}

/// Chat Completions 流（`Stream<Item = Result<StreamingChoice, OpenAiCompatError>>`）。
pub struct CompletionStream {
  inner: Pin<Box<dyn Stream<Item = Result<StreamingChoice, OpenAiCompatError>> + Send>>,
}

impl Stream for CompletionStream {
  type Item = Result<StreamingChoice, OpenAiCompatError>;

  fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
    Pin::new(&mut self.get_mut().inner).poll_next(cx)
  }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StreamingFunction {
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StreamingToolCall {
  pub index: usize,
  pub id: Option<String>,
  pub function: StreamingFunction,
}

#[derive(Deserialize, Debug)]
struct StreamingDelta {
  #[serde(default)]
  content: Option<String>,
  #[serde(default, deserialize_with = "json_utils::null_or_vec")]
  tool_calls: Vec<StreamingToolCall>,
}

#[derive(Deserialize, Debug)]
struct StreamingChoiceChunk {
  delta: StreamingDelta,
}

#[derive(Deserialize, Debug)]
struct StreamingCompletionChunk {
  #[serde(default)]
  choices: Vec<StreamingChoiceChunk>,
  #[serde(default)]
  usage: Option<Usage>,
}

impl CompletionModel {
  /// 流式 Chat Completions 调用（`stream` + `stream_options.include_usage` 由实现注入）。
  pub async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, OpenAiCompatError> {
    stream_chat_completions(&self.client, request).await
  }
}

pub(crate) async fn stream_chat_completions(
  client: &Client,
  request: CompletionRequest,
) -> Result<CompletionStream, OpenAiCompatError> {
  let model = request.model.clone();
  let mut request_as_json = serde_json::to_value(&request).map_err(OpenAiCompatError::from)?;
  request_as_json = merge(request_as_json, json!({"stream": true, "stream_options": {"include_usage": true}}));
  let req_body = serde_json::to_vec(&request_as_json).map_err(OpenAiCompatError::from)?;

  let response = client.post_json("/chat/completions", req_body).send().await?;
  if !response.status().is_success() {
    return Err(Client::error_from_response(response).await);
  }

  let event_source = response.bytes_stream().eventsource();

  let span = info_span!(target: "fusion_ai::completions", "chat_stream_inner");
  let s = stream! {
      let span = tracing::Span::current();
      let mut final_usage = Usage::new();

      // Track in-progress tool calls
      let mut tool_calls: HashMap<usize, (String, String, String)> = HashMap::new();

      let mut text_content = String::new();

      let mut event_source = std::pin::pin!(event_source);

      loop {
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

          // 终止形态一：OpenAI 兼容流以 `data: [DONE]` 结束
          if message.data.trim().is_empty() || message.data.trim() == "[DONE]" {
              continue;
          }

          let data = serde_json::from_str::<StreamingCompletionChunk>(&message.data);
          let Ok(data) = data else {
              let err = data.unwrap_err();
              tracing::debug!("Couldn't serialize data as StreamingCompletionChunk: {:?}", err);
              continue;
          };

          if let Some(choice) = data.choices.first() {
              let delta = &choice.delta;

              // Tool calls
              if !delta.tool_calls.is_empty() {
                  for tool_call in &delta.tool_calls {
                      let function = tool_call.function.clone();

                      // Start of tool call
                      if function.name.is_some() && function.arguments.is_empty() {
                          let id = tool_call.id.clone().unwrap_or_default();
                          tool_calls.insert(
                              tool_call.index,
                              (id, function.name.clone().unwrap(), "".to_string()),
                          );
                      }
                      // tool call partial (ie, a continuation of a previously received tool call)
                      else if function.name.clone().is_none_or(|s| s.is_empty())
                          && !function.arguments.is_empty()
                      {
                          if let Some((id, name, arguments)) = tool_calls.get(&tool_call.index).cloned() {
                              let new_arguments = &tool_call.function.arguments;
                              let combined_arguments = format!("{arguments}{new_arguments}");
                              tool_calls.insert(
                                tool_call.index,
                                (id.clone(), name.clone(), combined_arguments),
                              );

                              // Emit the delta so UI can show progress
                              yield Ok(StreamingChoice::ToolCallDelta {
                                id: id.clone(),
                                content: ToolCallDeltaContent::Delta(new_arguments.clone()),
                              });
                          } else {
                              tracing::debug!("Partial tool call received but tool call was never started.");
                          }
                      }
                      // Complete tool call
                      else {
                          let id = tool_call.id.clone().unwrap_or_default();
                          // Non-conformant providers may emit a chunk with neither
                          // name nor arguments (e.g. `{"function":{}}`); skip instead
                          // of panicking the stream task.
                          let Some(name) = function.name else {
                              tracing::debug!("Tool call chunk missing name and arguments; skipping");
                              continue;
                          };
                          let arguments = function.arguments;
                          let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&arguments) else {
                              tracing::debug!("Couldn't serialize '{arguments}' as JSON");
                              continue;
                          };

                          yield Ok(StreamingChoice::ToolCall {
                              id: id.clone(),
                              call_id: None,
                              name: name.clone(),
                              arguments: arguments.clone(),
                          });
                      }
                  }
              }

              // Message content
              if let Some(content) = &choice.delta.content {
                  text_content += content;
                  yield Ok(StreamingChoice::Text(content.clone()));
              }
          }

          // Usage updates
          if let Some(usage) = data.usage {
              final_usage = usage.clone();
          }
      }

      // Flush any tool calls that weren’t fully yielded
      for (_, (id, name, arguments)) in tool_calls {
          let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&arguments) else {
              continue;
          };

          yield Ok(StreamingChoice::ToolCall {
              id: id.clone(),
              call_id: None,
              name: name.clone(),
              arguments: arguments.clone(),
          });
      }

      span.record("gen_ai.usage.input_tokens", final_usage.prompt_tokens);
      span.record("gen_ai.usage.output_tokens", final_usage.total_tokens.saturating_sub(final_usage.prompt_tokens));
      span.record("gen_ai.request.model", &model);

      yield Ok(StreamingChoice::Final(StreamingCompletionResponse {
          usage: final_usage.clone()
      }));
  }
  .instrument(span);

  Ok(CompletionStream { inner: Box::pin(s) })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_streaming_function_deserialization() {
    let json = r#"{"name": "get_weather", "arguments": "{\"location\":\"Paris\"}"}"#;
    let function: StreamingFunction = serde_json::from_str(json).unwrap();
    assert_eq!(function.name, Some("get_weather".to_string()));
    assert_eq!(function.arguments, r#"{"location":"Paris"}"#.to_string());
  }

  #[test]
  fn test_streaming_tool_call_deserialization() {
    let json = r#"{
            "index": 0,
            "id": "call_abc123",
            "function": {
                "name": "get_weather",
                "arguments": "{\"city\":\"London\"}"
            }
        }"#;
    let tool_call: StreamingToolCall = serde_json::from_str(json).unwrap();
    assert_eq!(tool_call.index, 0);
    assert_eq!(tool_call.id, Some("call_abc123".to_string()));
    assert_eq!(tool_call.function.name, Some("get_weather".to_string()));
  }

  #[test]
  fn test_streaming_tool_call_partial_deserialization() {
    let json = r#"{
            "index": 0,
            "id": null,
            "function": {
                "name": null,
                "arguments": "Paris"
            }
        }"#;
    let tool_call: StreamingToolCall = serde_json::from_str(json).unwrap();
    assert_eq!(tool_call.index, 0);
    assert!(tool_call.id.is_none());
    assert!(tool_call.function.name.is_none());
    assert_eq!(tool_call.function.arguments, "Paris");
  }

  #[test]
  fn test_streaming_delta_with_tool_calls() {
    let json = r#"{
            "content": null,
            "tool_calls": [{
                "index": 0,
                "id": "call_xyz",
                "function": {
                    "name": "search",
                    "arguments": ""
                }
            }]
        }"#;
    let delta: StreamingDelta = serde_json::from_str(json).unwrap();
    assert!(delta.content.is_none());
    assert_eq!(delta.tool_calls.len(), 1);
    assert_eq!(delta.tool_calls[0].id, Some("call_xyz".to_string()));
  }

  #[test]
  fn test_streaming_chunk_deserialization() {
    let json = r#"{
            "choices": [{
                "delta": {
                    "content": "Hello",
                    "tool_calls": []
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;
    let chunk: StreamingCompletionChunk = serde_json::from_str(json).unwrap();
    assert_eq!(chunk.choices.len(), 1);
    assert_eq!(chunk.choices[0].delta.content, Some("Hello".to_string()));
    assert!(chunk.usage.is_some());
  }

  #[test]
  fn test_streaming_chunk_with_multiple_tool_call_deltas() {
    let json_start = r#"{
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_123",
                        "function": {
                            "name": "get_weather",
                            "arguments": ""
                        }
                    }]
                }
            }],
            "usage": null
        }"#;

    let json_chunk1 = r#"{
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": null,
                        "function": {
                            "name": null,
                            "arguments": "{\"loc"
                        }
                    }]
                }
            }],
            "usage": null
        }"#;

    let json_chunk2 = r#"{
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": null,
                        "function": {
                            "name": null,
                            "arguments": "ation\":\"NYC\"}"
                        }
                    }]
                }
            }],
            "usage": null
        }"#;

    // Verify each chunk deserializes correctly
    let start_chunk: StreamingCompletionChunk = serde_json::from_str(json_start).unwrap();
    assert_eq!(start_chunk.choices[0].delta.tool_calls.len(), 1);
    assert_eq!(start_chunk.choices[0].delta.tool_calls[0].function.name.as_ref().unwrap(), "get_weather");

    let chunk1: StreamingCompletionChunk = serde_json::from_str(json_chunk1).unwrap();
    assert_eq!(chunk1.choices[0].delta.tool_calls.len(), 1);
    assert_eq!(chunk1.choices[0].delta.tool_calls[0].function.arguments, "{\"loc");

    let chunk2: StreamingCompletionChunk = serde_json::from_str(json_chunk2).unwrap();
    assert_eq!(chunk2.choices[0].delta.tool_calls.len(), 1);
    assert_eq!(chunk2.choices[0].delta.tool_calls[0].function.arguments, "ation\":\"NYC\"}");
  }
}
