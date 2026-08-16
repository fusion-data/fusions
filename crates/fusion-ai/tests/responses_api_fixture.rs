//! Responses API 行为基线 fixture（fusion-ai-de-rig.md P0 / P2，本地 API）。
//!
//! 端点方言覆盖：OpenAI 官方（请求形状 + 非流式终态）、DashScope（assistant
//! content 恒序列化）、DeepSeek（流式无 `[DONE]`、以 `response.completed` 终止）。
//! 与 Chat Completions fixture 一起构成 SSE 终止双形态覆盖。

mod fixture_common;

use fixture_common::{API_KEY, request_body, responses_request, sse_body};
use futures::StreamExt;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use fusion_ai::providers::openai_compatible::Client;
use fusion_ai::providers::openai_compatible::errors::OpenAiCompatError;
use fusion_ai::providers::openai_compatible::responses_api::streaming::StreamingChoice;
use fusion_ai::providers::openai_compatible::responses_api::{
  CompletionResponse, Output, OutputMessage, ResponseStatus, ResponsesUsage,
};
use fusion_ai::providers::openai_compatible::types as core;

/// 构造指向 mock server 的 Responses 模型（`completion_model` 默认即 Responses 形态）。
async fn responses_model(
  server: &MockServer,
  model: &str,
) -> fusion_ai::providers::openai_compatible::responses_api::ResponsesCompletionModel {
  let client = Client::builder(API_KEY).base_url(server.uri().as_str()).build();
  client.completion_model(model)
}

// ================================================================
// OpenAI 官方方言：请求形状 + 非流式终态解析
// ================================================================

#[tokio::test]
async fn openai_responses_request_shape_and_non_streaming_parse() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "resp_Abc123",
        "object": "response",
        "created_at": 1723600000,
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "instructions": "You are a helpful assistant",
        "max_output_tokens": null,
        "model": "gpt-4o-2024-08-06",
        "usage": {
            "input_tokens": 25,
            "input_tokens_details": {"cached_tokens": 8},
            "output_tokens": 18,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 43
        },
        "output": [
            {
                "type": "message",
                "id": "msg_001",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "Hello! How can I help?"}]
            }
        ],
        "tools": []
    })))
    .expect(1)
    .mount(&server)
    .await;

  let request = responses_request(
    "gpt-4o",
    Some("You are a helpful assistant"),
    vec![core::Message::user("Hello")],
    vec![],
    None,
    Some(0.7),
    None,
    // Responses 形态的 thinking 控制走 reasoning 参数（extra-body 注入点）
    Some(json!({"reasoning": {"effort": "low"}})),
  );

  let model = responses_model(&server, "gpt-4o").await;
  let response: CompletionResponse = model.completion(request).await.expect("completion succeeds");

  // —— 请求体形状（golden）——
  let body = request_body(&server).await;
  assert_eq!(body["model"], "gpt-4o");
  assert_eq!(body["instructions"], "You are a helpful assistant");
  assert_eq!(body["temperature"], 0.7);
  assert_eq!(body["reasoning"]["effort"], "low");
  let input = body["input"].as_array().unwrap();
  assert_eq!(input[0]["role"], "user");
  assert_eq!(input[0]["type"], "message");
  assert_eq!(input[0]["content"][0]["type"], "input_text");
  assert_eq!(input[0]["content"][0]["text"], "Hello");
  // 非流式请求不携带 stream 字段
  assert!(body.get("stream").is_none());

  // —— 非流式响应解析（本地类型字段）——
  assert_eq!(response.id, "resp_Abc123");
  assert_eq!(response.status, ResponseStatus::Completed);
  assert_eq!(response.output.len(), 1);
  let usage: &ResponsesUsage = response.usage.as_ref().expect("usage present");
  assert_eq!((usage.input_tokens, usage.output_tokens, usage.total_tokens), (25, 18, 43));
  assert_eq!(usage.input_tokens_details.as_ref().unwrap().cached_tokens, 8);
  match &response.output[0] {
    Output::Message(OutputMessage { id, content, .. }) => {
      assert_eq!(id, "msg_001");
      assert_eq!(content.len(), 1);
    }
    other => panic!("expected message output, got {other:?}"),
  }
  // 便捷访问器
  assert_eq!(response.text().as_deref(), Some("Hello! How can I help?"));
  assert_eq!(response.usage_tokens().total_tokens, 43);
  assert_eq!(response.usage_tokens().cached_input_tokens, 8, "cached tokens must surface via usage_tokens()");
}

// ================================================================
// DashScope 方言：assistant history 的 content 恒序列化（400 坑回归）
// ================================================================

#[tokio::test]
async fn dashscope_responses_assistant_content_always_serialized() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "resp_qwen01",
        "object": "response",
        "created_at": 1723600001,
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "model": "qwen-max",
        "usage": {"input_tokens": 10, "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 15},
        "output": [
            {"type": "message", "id": "msg_q1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "你好"}]}
        ],
        "tools": []
    })))
    .expect(1)
    .mount(&server)
    .await;

  let history = vec![
    // Responses 形态下 assistant 回放消息必须携带 provider 侧 ID
    core::Message::assistant_with_id("msg_prev".into(), "之前的问题"),
    core::Message::user("继续"),
  ];
  let request = responses_request("qwen-max", None, history, vec![], None, None, None, None);

  let model = responses_model(&server, "qwen-max").await;
  let response = model.completion(request).await.expect("completion succeeds");

  let body = request_body(&server).await;
  let input = body["input"].as_array().unwrap();
  // DashScope Responses 要求 assistant 消息 content 字段必须存在（缺失 → 400）
  assert!(input[0].get("content").is_some(), "assistant content must always serialize: {input:?}");
  assert_eq!(input[0]["role"], "assistant");
  assert_eq!(input[0]["id"], "msg_prev");
  assert_eq!(input[0]["content"][0]["type"], "output_text");

  assert_eq!(response.usage.as_ref().unwrap().total_tokens, 15);
  assert_eq!(response.text().as_deref(), Some("你好"));
}

// ================================================================
// DeepSeek 方言：流式无 [DONE]，以 response.completed 终止
// ================================================================

#[tokio::test]
async fn deepseek_responses_streaming_terminated_by_completed_event() {
  let server = MockServer::start().await;
  let sse = sse_body(&[
    r#"{"type":"response.output_text.delta","item_id":"msg_ds1","output_index":0,"content_index":0,"sequence_number":4,"delta":"你好"}"#,
    r#"{"type":"response.output_text.delta","item_id":"msg_ds1","output_index":0,"content_index":0,"sequence_number":5,"delta":"，世界"}"#,
    r#"{"type":"response.completed","sequence_number":9,"response":{"id":"resp_ds01","object":"response","created_at":1723600002,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"max_output_tokens":null,"model":"deepseek-chat","usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":0},"output_tokens":6,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":18},"output":[{"type":"message","id":"msg_ds1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"你好，世界"}]}],"tools":[]}}"#,
  ]);
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
    .expect(1)
    .mount(&server)
    .await;

  let request =
    responses_request("deepseek-chat", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = responses_model(&server, "deepseek-chat").await;
  let mut stream = model.stream(request).await.expect("stream starts");

  let mut text = String::new();
  let mut final_usage: Option<(u64, u64)> = None;
  while let Some(choice) = stream.next().await {
    match choice.expect("chunk ok") {
      StreamingChoice::Text(delta) => text.push_str(&delta),
      StreamingChoice::Final(final_response) => {
        final_usage = Some((final_response.usage.input_tokens, final_response.usage.total_tokens));
      }
      _ => {}
    }
  }
  // 流没有 [DONE] 哨兵，终态由 response.completed 事件携带
  assert_eq!(text, "你好，世界");
  assert_eq!(final_usage.expect("final usage"), (12, 18));

  let body = request_body(&server).await;
  assert_eq!(body["stream"], true);
}

// ================================================================
// 流式终态 cached_tokens 透传（Qwen Responses 隐式缓存）
// ================================================================

#[tokio::test]
async fn responses_streaming_final_usage_carries_cached_tokens() {
  let server = MockServer::start().await;
  let sse = sse_body(&[
    r#"{"type":"response.output_text.delta","item_id":"msg_q2","output_index":0,"content_index":0,"sequence_number":4,"delta":"你好"}"#,
    r#"{"type":"response.completed","sequence_number":9,"response":{"id":"resp_qwen_cache","object":"response","created_at":1723600010,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"max_output_tokens":null,"model":"qwen3.7-plus","usage":{"input_tokens":700,"input_tokens_details":{"cached_tokens":560},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":730},"output":[{"type":"message","id":"msg_q2","role":"assistant","status":"completed","content":[{"type":"output_text","text":"你好"}]}],"tools":[]}}"#,
  ]);
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
    .expect(1)
    .mount(&server)
    .await;

  let request =
    responses_request("qwen3.7-plus", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = responses_model(&server, "qwen3.7-plus").await;
  let mut stream = model.stream(request).await.expect("stream starts");

  let mut final_cached: Option<(u64, u64)> = None;
  while let Some(choice) = stream.next().await {
    if let StreamingChoice::Final(final_response) = choice.expect("chunk ok") {
      let usage = final_response.usage_tokens();
      final_cached = Some((usage.input_tokens, usage.cached_input_tokens));
    }
  }
  assert_eq!(final_cached.expect("final usage"), (700, 560), "streaming final usage carries cached tokens");
}

// ================================================================
// 流式 reasoning 事件 + 工具调用终态（OpenAI Responses 方言）
// ================================================================

#[tokio::test]
async fn openai_responses_streaming_reasoning_and_tool_call() {
  let server = MockServer::start().await;
  let sse = sse_body(&[
    r#"{"type":"response.output_item.done","output_index":0,"sequence_number":3,"item":{"type":"reasoning","id":"rs_001","summary":[]}}"#,
    r#"{"type":"response.output_item.done","output_index":1,"sequence_number":7,"item":{"type":"function_call","id":"fc_001","call_id":"call_fc1","name":"get_weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}}"#,
    r#"{"type":"response.completed","sequence_number":9,"response":{"id":"resp_tool01","object":"response","created_at":1723600004,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"max_output_tokens":null,"model":"gpt-4o","usage":{"input_tokens":20,"input_tokens_details":{"cached_tokens":0},"output_tokens":10,"output_tokens_details":{"reasoning_tokens":4},"total_tokens":30},"output":[],"tools":[]}}"#,
  ]);
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream"))
    .expect(1)
    .mount(&server)
    .await;

  let request =
    responses_request("gpt-4o", None, vec![core::Message::user("Weather in Paris?")], vec![], None, None, None, None);
  let model = responses_model(&server, "gpt-4o").await;
  let mut stream = model.stream(request).await.expect("stream starts");

  let mut reasoning: Option<(Option<String>, String)> = None;
  let mut tool_call: Option<(String, String, serde_json::Value)> = None;
  let mut final_usage: Option<u64> = None;
  while let Some(choice) = stream.next().await {
    match choice.expect("chunk ok") {
      StreamingChoice::Reasoning { id, content } => reasoning = Some((id, content)),
      StreamingChoice::ToolCall { id, call_id, name, arguments } => {
        assert_eq!(call_id.as_deref(), Some("call_fc1"));
        tool_call = Some((id, name, arguments));
      }
      StreamingChoice::Final(final_response) => final_usage = Some(final_response.usage.total_tokens),
      _ => {}
    }
  }
  assert_eq!(reasoning, Some((Some("rs_001".into()), "".into())));
  let (id, name, arguments) = tool_call.expect("tool call event");
  assert_eq!(id, "fc_001");
  assert_eq!(name, "get_weather");
  assert_eq!(arguments, json!({"city": "Paris"}));
  assert_eq!(final_usage, Some(30));
}

// ================================================================
// 错误体：非 2xx（OpenAiCompatError::Http 分级）
// ================================================================

#[tokio::test]
async fn responses_error_responses_classified_by_status() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(500).set_body_string(r#"{"error":{"message":"Internal provider error"}}"#))
    .expect(1)
    .mount(&server)
    .await;

  let request = responses_request("any-model", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let model = responses_model(&server, "any-model").await;
  let err = model.completion(request).await.expect_err("non-2xx must fail");
  match &err {
    OpenAiCompatError::Http { status, message } => {
      assert_eq!(*status, 500);
      assert!(message.contains("Internal provider error"), "provider body must surface, got: {message}");
    }
    other => panic!("expected Http error, got {other:?}"),
  }
  assert!(err.is_upstream_transient());
}

// ================================================================
// user 多模态输入：input_image 方言
// ================================================================

#[tokio::test]
async fn responses_user_image_input_uses_input_image_dialect() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "resp_img01",
        "object": "response",
        "created_at": 1723600003,
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "model": "gpt-4o",
        "usage": {"input_tokens": 100, "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 105},
        "output": [
            {"type": "message", "id": "msg_i1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "A boardwalk."}]}
        ],
        "tools": []
    })))
    .expect(1)
    .mount(&server)
    .await;

  let history = vec![core::Message::User {
    content: core::OneOrMany::many(vec![
      core::UserContent::text("What's in this image?"),
      core::UserContent::image_url("https://example.test/x.jpg", None, None),
    ])
    .unwrap(),
  }];
  let request = responses_request("gpt-4o", None, history, vec![], None, None, None, None);

  let model = responses_model(&server, "gpt-4o").await;
  model.completion(request).await.expect("completion succeeds");

  let body = request_body(&server).await;
  let input = body["input"].as_array().unwrap();
  // Responses 形态：一条 user 消息的多模态 content 拆成独立 input item（各携带单一 content）
  assert_eq!(input.len(), 2);
  assert_eq!(input[0]["content"][0]["type"], "input_text");
  assert_eq!(input[1]["content"][0]["type"], "input_image");
  assert_eq!(input[1]["content"][0]["image_url"], "https://example.test/x.jpg");
}

// ================================================================
// max_output_tokens 一等字段（max_tokens → max_output_tokens 映射）
// ================================================================

#[tokio::test]
async fn responses_max_tokens_maps_to_max_output_tokens() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/responses"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id": "resp_mt01", "object": "response", "created_at": 1723600005, "status": "completed",
        "error": null, "incomplete_details": null, "instructions": null, "max_output_tokens": 4096,
        "model": "qwen-max",
        "usage": {"input_tokens": 5, "output_tokens": 2, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 7},
        "output": [{"type": "message", "id": "msg_mt", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "ok"}]}],
        "tools": []
    })))
    .expect(1)
    .mount(&server)
    .await;

  let request =
    responses_request("qwen-max", None, vec![core::Message::user("Hi")], vec![], None, None, Some(4096), None);
  let model = responses_model(&server, "qwen-max").await;
  model.completion(request).await.expect("completion succeeds");

  let body = request_body(&server).await;
  assert_eq!(body["max_output_tokens"], 4096);
}
