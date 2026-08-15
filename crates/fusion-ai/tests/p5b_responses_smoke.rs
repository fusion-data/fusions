//! P5b 真机 gate（fusion-ai-de-rig.md §P5b）：DeepSeek / Qwen 切 Responses 前的
//! 真机 smoke——非流式 + 流式各一次，验证 thinking 关闭参数与 usage / 延迟。
//!
//! 运行（需真机 key）：
//! ```text
//! DEEPSEEK_API_KEY=sk-... DASHSCOPE_API_KEY=sk-... \
//!   cargo test -p fusion-ai --test p5b_responses_smoke -- --ignored --nocapture
//! ```
//! Kimi（Moonshot）仅支持 chat completions，不在本 smoke 范围（保持 Chat Completions）。

#![allow(clippy::too_many_arguments)]

use futures::StreamExt;
use std::time::Instant;

use fusion_ai::providers::openai_compatible::responses_api::streaming::StreamingChoice;
use fusion_ai::providers::openai_compatible::responses_api::CompletionRequest;
use fusion_ai::providers::openai_compatible::types as core;
use fusion_ai::providers::openai_compatible::{Client};

/// 单端点 smoke：非流式 chat + 流式 stream，返回 (text, usage 描述, 延迟)。
async fn smoke_endpoint(name: &str, base_url: &str, api_key: &str, model: &str, reasoning_off: serde_json::Value) {
  let client = Client::builder(api_key).base_url(base_url).build();
  let responses_model = client.completion_model(model);

  // —— 非流式：reasoning.effort=none 关思考（DashScope 官方口径；DeepSeek 真机验证点）——
  let request = CompletionRequest::from_history(
    model,
    Some("你是一个简洁的助手".to_string()),
    vec![core::Message::user("用一句话介绍 Rust 语言")],
    vec![],
    None,
    None,
    Some(2048),
    Some(reasoning_off.clone()),
  )
  .expect("request builds");

  let start = Instant::now();
  match responses_model.completion(request).await {
    Ok(response) => {
      let latency = start.elapsed();
      let usage = response
        .usage
        .as_ref()
        .map(|u| format!("input={} output={} total={} reasoning_tokens={}", u.input_tokens, u.output_tokens, u.total_tokens, u.output_tokens_details.reasoning_tokens))
        .unwrap_or_else(|| "None".into());
      println!(
          "[{name}] NON-STREAM ok: latency={:?} status={:?} text={:?}\n[{name}] usage: {usage}",
          latency, response.status, response.text()
      );
    }
    Err(e) => {
      println!("[{name}] NON-STREAM FAILED: {e}");
      panic!("[{name}] responses non-stream smoke failed: {e}");
    }
  }

  // —— 流式：终态由 response.completed 携带（无 [DONE]）——
  let request = CompletionRequest::from_history(
    model,
    None,
    vec![core::Message::user("用两句话解释什么是所有权")],
    vec![],
    None,
    None,
    Some(2048),
    Some(reasoning_off.clone()),
  )
  .expect("request builds");

  let start = Instant::now();
  match responses_model.stream(request).await {
    Ok(mut stream) => {
      let mut text = String::new();
      let mut chunks = 0u32;
      let mut final_usage = String::new();
      while let Some(choice) = stream.next().await {
        match choice.expect("chunk ok") {
          StreamingChoice::Text(delta) => {
            text.push_str(&delta);
            chunks += 1;
          }
          StreamingChoice::Final(final_response) => {
            final_usage = format!(
              "input={} total={}",
              final_response.usage.input_tokens, final_response.usage.total_tokens
            );
          }
          _ => {}
        }
      }
      println!(
          "[{name}] STREAM ok: latency={:?} chunks={chunks} text={:?}\n[{name}] final usage: {final_usage}",
          start.elapsed(), text
      );
      assert!(!text.is_empty(), "[{name}] stream text must not be empty");
      assert!(!final_usage.is_empty(), "[{name}] stream must carry final usage (response.completed)");
    }
    Err(e) => {
      println!("[{name}] STREAM FAILED: {e}");
      panic!("[{name}] responses stream smoke failed: {e}");
    }
  }
}

#[tokio::test]
#[ignore = "P5b 真机 gate：需 DEEPSEEK_API_KEY，chat + stream smoke（fusion-ai-de-rig.md §P5b）"]
async fn deepseek_responses_smoke() {
  let api_key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
  // DeepSeek Responses 的 effort 子集为 minimal/low/medium/high（无 none）——minimal 即最接近关闭
  smoke_endpoint(
    "deepseek",
    "https://api.deepseek.com",
    &api_key,
    "deepseek-v4-flash",
    serde_json::json!({"reasoning": {"effort": "minimal"}}),
  )
  .await;
}

#[tokio::test]
#[ignore = "P5b 真机 gate：需 DASHSCOPE_API_KEY，chat + stream smoke（fusion-ai-de-rig.md §P5b）"]
async fn qwen_responses_smoke() {
  let api_key = std::env::var("DASHSCOPE_API_KEY").expect("DASHSCOPE_API_KEY not set");
  // DashScope Responses 官方口径：reasoning.effort="none" 关闭思考
  smoke_endpoint(
    "qwen",
    "https://dashscope.aliyuncs.com/compatible-mode/v1",
    &api_key,
    "qwen3.7-plus",
    serde_json::json!({"reasoning": {"effort": "none"}}),
  )
  .await;
}
