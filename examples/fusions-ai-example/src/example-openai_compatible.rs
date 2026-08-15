use fusions::ai::AiError;
use fusions::ai::providers::openai_compatible::completion::{CompletionModel, CompletionRequest};
use fusions::ai::providers::openai_compatible::types as core_types;

/// 示例：使用 OpenAI 兼容 API 调用模型（本地 wire，无 rig）
///
/// `RUST_LOG=debug cargo run -p fusions-ai-example --bin example-openai-compat`
#[tokio::main]
async fn main() -> Result<(), AiError> {
  dotenvy::dotenv().unwrap();
  logforth::starter_log::stdout().apply();

  // 可切换的端点（Kimi / DeepSeek / SiliconFlow / 智谱）：Moonshot 等仅支持
  // chat completions 的端点必须走 `chat_completions_model`（fusion-ai-de-rig.md §4.1）
  let base_url = "https://ai.gitee.com/v1";
  let api_key = std::env::var("GITEE_AI_API_KEY").unwrap();
  let model_name = "Kimi-K2-Thinking";

  let client = fusions::ai::providers::openai_compatible::Client::builder(&api_key).base_url(base_url).build();
  let model: CompletionModel = client.chat_completions_model(model_name);

  let request = CompletionRequest::from_history(
    model.model(),
    Some("你是一个 AI 助手".to_string()),
    vec![core_types::Message::user("你是谁？")],
    vec![],
    None,
    Some(0.7),
    None,
    None,
  )?;

  let response = model.completion(request).await?;

  println!("Response usage: {:?}", response.usage);
  println!("Response text: {}", response.text().unwrap_or_default());
  Ok(())
}
