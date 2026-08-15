//! openai_compatible 行为基线 fixture 的公共工具（fusion-ai-de-rig.md P0）。
//!
//! 断言纪律：所有断言钉在 **wire 层**（wiremock 收到的请求 JSON / SSE 帧序列）与
//! **本地响应类型**（`CompletionResponse` / `Usage` / `StreamingCompletionResponse`
//! 等本地化后的字段），P1-P3 重构期间断言未动即证明行为等价。
//!
//! 按端点方言组织样例（OpenAI 官方 / DashScope / DeepSeek / Kimi 各一），既测
//! 「我们发的请求形状」，也测各端点响应方言的解析。

#![allow(dead_code)]

/// fixture 统一密钥：携密断言都盯它
pub const API_KEY: &str = "sk-fixture-secret-key-0123456789";

/// SSE 帧序列（`data: <chunk>` 形式，帧间空行分隔）。
pub fn sse_body(chunks: &[&str]) -> String {
  let mut body = String::new();
  for chunk in chunks {
    body.push_str("data: ");
    body.push_str(chunk);
    body.push_str("\n\n");
  }
  body
}

/// 构造本地 Chat Completions 请求（P1 起的公共构造面）。
#[allow(clippy::too_many_arguments)]
pub fn chat_request(
  model: &str,
  preamble: Option<&str>,
  history: Vec<fusion_ai::providers::openai_compatible::types::Message>,
  tools: Vec<fusion_ai::providers::openai_compatible::types::ToolDefinition>,
  tool_choice: Option<fusion_ai::providers::openai_compatible::types::ToolChoice>,
  temperature: Option<f64>,
  max_tokens: Option<u64>,
  additional_params: Option<serde_json::Value>,
) -> fusion_ai::providers::openai_compatible::completion::CompletionRequest {
  fusion_ai::providers::openai_compatible::completion::CompletionRequest::from_history(
    model,
    preamble.map(Into::into),
    history,
    tools,
    tool_choice,
    temperature,
    max_tokens,
    additional_params,
  )
  .expect("chat request builds")
}

/// 构造本地 Responses 请求（P2 起的公共构造面）。
#[allow(clippy::too_many_arguments)]
pub fn responses_request(
  model: &str,
  preamble: Option<&str>,
  history: Vec<fusion_ai::providers::openai_compatible::types::Message>,
  tools: Vec<fusion_ai::providers::openai_compatible::types::ToolDefinition>,
  tool_choice: Option<fusion_ai::providers::openai_compatible::types::ToolChoice>,
  temperature: Option<f64>,
  max_tokens: Option<u64>,
  additional_params: Option<serde_json::Value>,
) -> fusion_ai::providers::openai_compatible::responses_api::CompletionRequest {
  fusion_ai::providers::openai_compatible::responses_api::CompletionRequest::from_history(
    model,
    preamble.map(Into::into),
    history,
    tools,
    tool_choice,
    temperature,
    max_tokens,
    additional_params,
  )
  .expect("responses request builds")
}

/// wiremock 收到的请求体解析为 JSON（请求断言统一入口）。
pub async fn request_body(server: &wiremock::MockServer) -> serde_json::Value {
  let requests = server.received_requests().await.expect("request recorded");
  let request = &requests[0];
  serde_json::from_slice(&request.body).expect("request body is valid JSON")
}
