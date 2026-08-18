//! usage cache 双方言判据单点 fixture —— 两套 OpenAI 兼容 wire 对**同组输入**断言一致。
//!
//! 判据唯一实现 = `providers::openai_compatible::completion::cache_hit_input_tokens`
//! （DeepSeek flat `prompt_cache_hit_tokens` / OpenAI 嵌套
//! `prompt_tokens_details.cached_tokens`，同时出现取 max，皆无 → 0）。
//! 本文件把同组 usage payload 同时喂 `providers::openai_compatible`（`CompletionModel`，
//! 非流式）与 `llm::wire_openai_compat`（`OpenAiCompatTransport`），两套 wire 的 cache
//! 命中数 MUST 相等且等于期望值 —— 双 wire 同源由本测试机器钉死，不靠审查纪律。
//! 流式终态同判据（同一 `Usage::deserialize`）由 `chat_completions_fixture.rs` 的
//! `deepseek_streaming_final_usage_carries_cache_hit_tokens` 覆盖。

mod fixture_common;

use fixture_common::API_KEY;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use fusion_ai::llm::wire_openai_compat::OpenAiCompatTransport;
use fusion_ai::llm::{ChatCompletionRequest, ChatMessage, LlmProviderId};
use fusion_ai::providers::openai_compatible::Client;
use fusion_ai::providers::openai_compatible::types as core;

/// (说明, usage JSON, 期望 cache 命中数) —— payload 与
/// `chat_completions_fixture.rs` 的 cache 用例同组，另补「双方言并存取 max」。
fn cache_cases() -> Vec<(&'static str, Value, u64)> {
  vec![
    (
      "deepseek flat prompt_cache_hit_tokens",
      json!({"prompt_tokens": 1000, "completion_tokens": 50, "total_tokens": 1050,
             "prompt_cache_hit_tokens": 800, "prompt_cache_miss_tokens": 200}),
      800,
    ),
    (
      "openai nested prompt_tokens_details.cached_tokens",
      json!({"prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940,
             "prompt_tokens_details": {"cached_tokens": 600}}),
      600,
    ),
    ("no cache fields degrades to zero", json!({"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}), 0),
    (
      "both dialects present takes max",
      json!({"prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940,
             "prompt_cache_hit_tokens": 100, "prompt_tokens_details": {"cached_tokens": 600}}),
      600,
    ),
  ]
}

fn completion_body(usage: &Value) -> Value {
  json!({
    "id": "chatcmpl-cache", "object": "chat.completion", "created": 1723600099, "model": "m",
    "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
    "usage": usage
  })
}

async fn mount_once(server: &MockServer, body: Value) {
  Mock::given(method("POST"))
    .and(path("/chat/completions"))
    .respond_with(ResponseTemplate::new(200).set_body_json(body))
    .expect(1)
    .mount(server)
    .await;
}

/// `providers::openai_compatible` wire（非流式 CompletionModel）的 cache 命中数。
async fn openai_compatible_cached(server: &MockServer) -> u64 {
  let client = Client::builder(API_KEY).base_url(server.uri().as_str()).build();
  let model = client.completion_model("m").completions_api();
  let request =
    fixture_common::chat_request("m", None, vec![core::Message::user("Hi")], vec![], None, None, None, None);
  let response = model.completion(request).await.expect("completion succeeds");
  response.usage_tokens().cached_input_tokens
}

/// `llm::wire_openai_compat` wire（OpenAiCompatTransport）的 cache 命中数。
async fn wire_openai_compat_cached(server: &MockServer) -> u64 {
  let transport = OpenAiCompatTransport::new(server.uri(), API_KEY).expect("transport builds");
  let req = ChatCompletionRequest { messages: vec![ChatMessage::user("Hi")], ..Default::default() };
  let resp = transport.chat_complete(LlmProviderId::DeepSeek, "m", req).await.expect("chat_complete succeeds");
  resp.usage.expect("usage present").cached_input_tokens as u64
}

#[tokio::test]
async fn both_wires_agree_on_cache_dialect_inputs() {
  for (name, usage_json, expected) in cache_cases() {
    let server_a = MockServer::start().await;
    mount_once(&server_a, completion_body(&usage_json)).await;
    let a = openai_compatible_cached(&server_a).await;

    let server_b = MockServer::start().await;
    mount_once(&server_b, completion_body(&usage_json)).await;
    let b = wire_openai_compat_cached(&server_b).await;

    assert_eq!(a, expected, "openai_compatible wire: {name}");
    assert_eq!(b, expected, "wire_openai_compat wire: {name}");
    assert_eq!(a, b, "two wires MUST agree on the same input: {name}");
  }
}
