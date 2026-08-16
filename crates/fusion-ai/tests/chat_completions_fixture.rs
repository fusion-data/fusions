//! Chat Completions 行为基线 fixture（fusion-ai-de-rig.md P0，P1 起走本地 API）。
//!
//! 端点方言覆盖：OpenAI 官方（请求形状 + 非流式）、DashScope（assistant content
//! 恒序列化 + vision 方言）、DeepSeek（流式 tool call 聚合）、Kimi（流式 text delta
//! + `[DONE]` 终止）。断言钉 wire 层与本地响应类型。

mod fixture_common;

use fixture_common::{API_KEY, chat_request, request_body, sse_body};
use futures::StreamExt;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use fusion_ai::providers::openai_compatible::completion::streaming::{StreamingChoice, ToolCallDeltaContent};
use fusion_ai::providers::openai_compatible::errors::OpenAiCompatError;
use fusion_ai::providers::openai_compatible::types as core;
use fusion_ai::providers::openai_compatible::types::ToolChoice as CoreToolChoice;
use fusion_ai::providers::openai_compatible::{AssistantContent, Client, Message};

/// 构造指向 mock server 的 Chat Completions 模型（Kimi 等端点形态：显式 completions_api）。
async fn chat_model(
  server: &MockServer,
  model: &str,
) -> fusion_ai::providers::openai_compatible::completion::CompletionModel {
  let client = Client::builder(API_KEY).base_url(server.uri().as_str()).build();
  client.completion_model(model).completions_api()
}

// ================================================================
// OpenAI 官方方言：请求形状 + 非流式解析
// ================================================================

#[tokio::test]
async fn openai_official_request_shape_and_non_streaming_parse() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .and(header("authorization", format!("Bearer {API_KEY}").as_str()))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-Abc123",
        "object": "chat.completion",
        "created": 1723600000,
        "model": "gpt-4o-2024-08-06",
        "system_fingerprint": "fp_aaaa",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello! How can I help you today?"},
            "logprobs": null,
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 9, "completion_tokens": 12, "total_tokens": 21}
    })))
    .expect(1)
    .mount(&server)
    .await;

  let history = vec![core::Message::assistant("Hi there!"), core::Message::user("Hello")];
  let tools = vec![core::ToolDefinition {
    name: "get_weather".into(),
    parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    description: "Get current weather".into(),
  }];
  let request = chat_request(
    "gpt-4o",
    Some("You are a helpful assistant"),
    history,
    tools,
    Some(CoreToolChoice::Required),
    Some(0.5),
    None,
    // extra-body 注入点：DeepSeek thinking 关闭依赖此通道
    Some(json!({"thinking": {"type": "disabled"}})),
  );

  let model = chat_model(&server, "gpt-4o").await;
  let response = model.completion(request).await.expect("completion succeeds");

  // —— 请求体形状（golden）——
  let body = request_body(&server).await;
  assert_eq!(body["model"], "gpt-4o");
  let messages = body["messages"].as_array().unwrap();
  // preamble → system 消息打头
  assert_eq!(messages[0]["role"], "system");
  assert_eq!(messages[0]["content"][0]["type"], "text");
  assert_eq!(messages[0]["content"][0]["text"], "You are a helpful assistant");
  assert_eq!(messages[1]["role"], "assistant");
  assert_eq!(messages[1]["content"][0]["type"], "text");
  assert_eq!(messages[1]["content"][0]["text"], "Hi there!");
  assert_eq!(messages[2]["role"], "user");
  assert_eq!(messages[2]["content"][0]["type"], "text");
  assert_eq!(messages[2]["content"][0]["text"], "Hello");
  // tools / tool_choice
  assert_eq!(body["tools"][0]["type"], "function");
  assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
  assert_eq!(body["tools"][0]["function"]["parameters"]["properties"]["city"]["type"], "string");
  assert_eq!(body["tool_choice"], "required");
  // temperature 与 additional_params（flatten 注入）
  assert_eq!(body["temperature"], 0.5);
  assert_eq!(body["thinking"]["type"], "disabled");

  // —— 非流式响应解析（本地类型字段）——
  assert_eq!(response.id, "chatcmpl-Abc123");
  assert_eq!(response.model, "gpt-4o-2024-08-06");
  assert_eq!(response.choices.len(), 1);
  assert_eq!(response.choices[0].finish_reason, "stop");
  match &response.choices[0].message {
    Message::Assistant { content, tool_calls, .. } => {
      assert_eq!(content[0], AssistantContent::Text { text: "Hello! How can I help you today?".into() });
      assert!(tool_calls.is_empty());
    }
    other => panic!("expected assistant message, got {other:?}"),
  }
  let usage = response.usage.clone().expect("usage present");
  assert_eq!((usage.prompt_tokens, usage.completion_tokens, usage.total_tokens), (9, 12, 21));
  // 便捷访问器
  assert_eq!(response.text().as_deref(), Some("Hello! How can I help you today?"));
  assert_eq!(response.usage_tokens().total_tokens, 21);
}

// ================================================================
// DashScope 兼容方言：assistant content 恒序列化（400 坑回归）+ vision 输入
// ================================================================

#[tokio::test]
async fn dashscope_assistant_content_always_serialized_and_vision_input() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        // DashScope compatible-mode 响应方言：字段超集被忽略，usage 形状一致
        "id": "cmpl-8f2a0b7c",
        "object": "chat.completion",
        "created": 1723600001,
        "model": "qwen-vl-max",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "The image shows a boardwalk."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 410, "completion_tokens": 8, "total_tokens": 418}
    })))
    .expect(1)
    .mount(&server)
    .await;

  // assistant 消息 content 为空（纯 tool call 回合的 history 复放形态）
  let history = vec![
    core::Message::assistant(""),
    core::Message::User {
      content: core::OneOrMany::many(vec![
        core::UserContent::text("What's in this image?"),
        core::UserContent::image_url("https://example.test/boardwalk.jpg", None, None),
      ])
      .unwrap(),
    },
  ];
  let request = chat_request("qwen-vl-max", None, history, vec![], None, None, None, None);

  let model = chat_model(&server, "qwen-vl-max").await;
  let response = model.completion(request).await.expect("completion succeeds");

  let body = request_body(&server).await;
  let messages = body["messages"].as_array().unwrap();
  // DashScope 兼容端要求 assistant content 字段存在（缺失 → 400）
  assert!(messages[0].get("content").is_some(), "assistant content must always serialize: {messages:?}");
  // vision user content → image_url 方言
  assert_eq!(messages[1]["content"][1]["type"], "image_url");
  assert_eq!(messages[1]["content"][1]["image_url"]["url"], "https://example.test/boardwalk.jpg");

  let usage = response.usage.clone().expect("usage present");
  assert_eq!((usage.prompt_tokens, usage.total_tokens), (410, 418));
  assert_eq!(response.text().as_deref(), Some("The image shows a boardwalk."));
}

// ================================================================
// DeepSeek 方言：流式 tool call 分片聚合
// ================================================================

#[tokio::test]
async fn deepseek_streaming_tool_call_accumulation() {
  let server = MockServer::start().await;
  let sse = sse_body(&[
    r#"{"id":"chatcmpl-ds01","object":"chat.completion.chunk","created":1723600002,"model":"deepseek-chat","choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_ds_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
    r#"{"id":"chatcmpl-ds01","object":"chat.completion.chunk","created":1723600002,"model":"deepseek-chat","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":null,"arguments":"{\"ci"}}]},"finish_reason":null}]}"#,
    r#"{"id":"chatcmpl-ds01","object":"chat.completion.chunk","created":1723600002,"model":"deepseek-chat","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":null,"arguments":"ty\":\"Paris\"}"}}]},"finish_reason":null}]}"#,
    r#"{"id":"chatcmpl-ds01","object":"chat.completion.chunk","created":1723600002,"model":"deepseek-chat","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":14,"completion_tokens":7,"total_tokens":21}}"#,
    "[DONE]",
  ]);
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
    .expect(1)
    .mount(&server)
    .await;

  let request =
    chat_request("deepseek-chat", None, vec![core::Message::user("Weather in Paris?")], vec![], None, None, None, None);
  let model = chat_model(&server, "deepseek-chat").await;
  let mut stream = model.stream(request).await.expect("stream starts");

  let mut tool_call_arguments: Option<serde_json::Value> = None;
  let mut final_usage: Option<(usize, usize)> = None;
  while let Some(choice) = stream.next().await {
    match choice.expect("chunk ok") {
      StreamingChoice::ToolCallDelta { content, .. } => {
        // 聚合过程中的参数分片
        let ToolCallDeltaContent::Delta(delta) = content else {
          panic!("expected delta content");
        };
        assert!(delta.contains("ci") || delta.contains("ty"));
      }
      StreamingChoice::ToolCall { name, arguments, .. } => {
        assert_eq!(name, "get_weather");
        tool_call_arguments = Some(arguments);
      }
      StreamingChoice::Final(final_response) => {
        final_usage = Some((final_response.usage.prompt_tokens, final_response.usage.total_tokens));
      }
      _ => {}
    }
  }

  assert_eq!(tool_call_arguments.expect("tool call accumulated"), json!({"city": "Paris"}));
  assert_eq!(final_usage.expect("final usage"), (14, 21));

  // 请求体：流式参数由实现注入
  let body = request_body(&server).await;
  assert_eq!(body["stream"], true);
  assert_eq!(body["stream_options"]["include_usage"], true);
}

// ================================================================
// Kimi 方言：流式 text delta + [DONE] 终止 + usage
// ================================================================

#[tokio::test]
async fn kimi_streaming_text_delta_with_done_sentinel() {
  let server = MockServer::start().await;
  let sse = sse_body(&[
    r#"{"id":"cmpl-moonshot01","object":"chat.completion.chunk","created":1723600003,"model":"moonshot-v1-8k","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
    r#"{"id":"cmpl-moonshot01","object":"chat.completion.chunk","created":1723600003,"model":"moonshot-v1-8k","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
    r#"{"id":"cmpl-moonshot01","object":"chat.completion.chunk","created":1723600003,"model":"moonshot-v1-8k","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":6,"completion_tokens":2,"total_tokens":8}}"#,
    "[DONE]",
  ]);
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
    .expect(1)
    .mount(&server)
    .await;

  let request = chat_request("moonshot-v1-8k", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = chat_model(&server, "moonshot-v1-8k").await;
  let mut stream = model.stream(request).await.expect("stream starts");

  let mut text = String::new();
  let mut final_usage: Option<(usize, usize)> = None;
  while let Some(choice) = stream.next().await {
    match choice.expect("chunk ok") {
      StreamingChoice::Text(delta) => text.push_str(&delta),
      StreamingChoice::Final(final_response) => {
        final_usage = Some((final_response.usage.prompt_tokens, final_response.usage.total_tokens));
      }
      _ => {}
    }
  }

  assert_eq!(text, "Hello world");
  assert_eq!(final_usage.expect("final usage"), (6, 8));
}

// ================================================================
// 错误体：4xx / 5xx 非 2xx 响应（OpenAiCompatError::Http 分级）
// ================================================================

#[tokio::test]
async fn chat_error_responses_classified_by_status() {
  for (status, transient) in [(429u16, true), (500u16, true), (400u16, false)] {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/chat/completions"))
      .respond_with(
        ResponseTemplate::new(status)
          .set_body_json(json!({"error": {"message": "provider exploded", "type": "server_error"}})),
      )
      .expect(1)
      .mount(&server)
      .await;

    let request = chat_request("any-model", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
    let model = chat_model(&server, "any-model").await;
    let err = model.completion(request).await.expect_err("non-2xx must fail");

    match &err {
      OpenAiCompatError::Http { status: got, message } => {
        assert_eq!(*got, status);
        assert!(message.contains("provider exploded"), "provider body must surface, got: {message}");
      }
      other => panic!("expected Http error, got {other:?}"),
    }
    assert_eq!(err.is_upstream_transient(), transient, "status {status}");
  }
}

// ================================================================
// max_tokens 一等字段（fork 基线经 rig CoreCompletionRequest 时被丢弃，本地化修复）
// ================================================================

#[tokio::test]
async fn chat_max_tokens_is_first_class_request_field() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-mt01", "object": "chat.completion", "created": 1723600004, "model": "deepseek-chat",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "length"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 8, "total_tokens": 11}
    })))
    .expect(1)
    .mount(&server)
    .await;

  let request =
    chat_request("deepseek-chat", None, vec![core::Message::user("Hi")], vec![], None, None, Some(8192), None);
  let model = chat_model(&server, "deepseek-chat").await;
  model.completion(request).await.expect("completion succeeds");

  let body = request_body(&server).await;
  assert_eq!(body["max_tokens"], 8192);
}

// ================================================================
// 携密纪律：Debug 输出不含 api_key
// ================================================================

#[test]
fn client_debug_never_leaks_api_key() {
  let client = Client::builder(API_KEY).build();
  let dbg = format!("{client:?}");
  assert!(!dbg.contains(API_KEY), "api_key leaked: {dbg}");
  assert!(dbg.contains("REDACTED"));
}

// ================================================================
// cache 命中 usage 双方言解析（DeepSeek flat / OpenAI 嵌套，两者并存取 max）
// ================================================================

#[tokio::test]
async fn deepseek_non_streaming_flat_cache_hit_tokens() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-ds-cache", "object": "chat.completion", "created": 1723600010, "model": "deepseek-chat",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "total_tokens": 1050,
            "prompt_cache_hit_tokens": 800,
            "prompt_cache_miss_tokens": 200
        }
    })))
    .expect(1)
    .mount(&server)
    .await;

  let request = chat_request("deepseek-chat", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = chat_model(&server, "deepseek-chat").await;
  let response = model.completion(request).await.expect("completion succeeds");

  let usage = response.usage_tokens();
  assert_eq!(usage.input_tokens, 1000);
  assert_eq!(usage.output_tokens, 50);
  assert_eq!(usage.cached_input_tokens, 800, "flat dialect prompt_cache_hit_tokens must surface");
}

#[tokio::test]
async fn openai_non_streaming_nested_cached_tokens() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-oai-cache", "object": "chat.completion", "created": 1723600011, "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 900,
            "completion_tokens": 40,
            "total_tokens": 940,
            "prompt_tokens_details": {"cached_tokens": 600}
        }
    })))
    .expect(1)
    .mount(&server)
    .await;

  let request = chat_request("gpt-4o", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = chat_model(&server, "gpt-4o").await;
  let response = model.completion(request).await.expect("completion succeeds");

  let usage = response.usage_tokens();
  assert_eq!(usage.input_tokens, 900);
  assert_eq!(usage.cached_input_tokens, 600, "nested dialect prompt_tokens_details.cached_tokens must surface");
}

#[tokio::test]
async fn non_streaming_usage_without_cache_fields_degrades_to_zero() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-nocache", "object": "chat.completion", "created": 1723600012, "model": "moonshot-v1-8k",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}
    })))
    .expect(1)
    .mount(&server)
    .await;

  let request = chat_request("moonshot-v1-8k", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = chat_model(&server, "moonshot-v1-8k").await;
  let response = model.completion(request).await.expect("completion succeeds");

  assert_eq!(
    response.usage_tokens().cached_input_tokens,
    0,
    "unknown dialect degrades to 0 (no worse than status quo)"
  );
}

#[tokio::test]
async fn deepseek_streaming_final_usage_carries_cache_hit_tokens() {
  let server = MockServer::start().await;
  let sse = sse_body(&[
    r#"{"id":"chatcmpl-ds-cache-s","object":"chat.completion.chunk","created":1723600013,"model":"deepseek-chat","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}"#,
    r#"{"id":"chatcmpl-ds-cache-s","object":"chat.completion.chunk","created":1723600013,"model":"deepseek-chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":500,"completion_tokens":20,"total_tokens":520,"prompt_cache_hit_tokens":400}}"#,
    "[DONE]",
  ]);
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
    .expect(1)
    .mount(&server)
    .await;

  let request = chat_request("deepseek-chat", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = chat_model(&server, "deepseek-chat").await;
  let mut stream = model.stream(request).await.expect("stream starts");

  let mut final_cached: Option<u64> = None;
  while let Some(choice) = stream.next().await {
    if let StreamingChoice::Final(final_response) = choice.expect("chunk ok") {
      final_cached = Some(final_response.usage_tokens().cached_input_tokens);
    }
  }
  assert_eq!(final_cached.expect("final usage"), 400, "streaming final usage carries cache hit tokens");
}
