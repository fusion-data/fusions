// Respond url
// .base_url("https://api.siliconflow.cn/v1")
// .api_key(std::env::var("SILICONFLOW_API_KEY").unwrap())
// .model("Kwai-Kolors/Kolors")

// Respond url
// .base_url("https://open.bigmodel.cn/api/coding/paas/v4")
// .api_key(std::env::var("ZAI_API_KEY").unwrap())
// .model("cogview-4-250304")

use fusions::ai::AiError;
use fusions::ai::providers::openai_compatible::image_generation::ImageGenerationRequest;
use fusions::ai::utils::vec_to_image_file;

/// 生成图片示例（本地 wire，无 rig）
///
/// `RUST_LOG=debug cargo run -p fusions-ai-example --bin example-gen-image`
#[tokio::main]
async fn main() -> Result<(), AiError> {
  dotenvy::dotenv().unwrap();
  logforth::starter_log::stdout().apply();

  let base_url = "https://ai.gitee.com/v1";
  let api_key = std::env::var("GITEE_AI_API_KEY").unwrap();
  let model_name = "flux-1-schnell";

  let client = fusions::ai::providers::openai_compatible::Client::builder(&api_key).base_url(base_url).build();
  let image_model = client.image_generation_model(model_name);

  let response = image_model
    .image_generation(
      ImageGenerationRequest::new(
        "使用 Rust, Python, Typescript 这 3 门编程语言的 logo 合成一个新的 logo。要求新 logo 能够让专业人士明确的分辨出包含有这 3 门编程语言的元素",
      )
      .with_size(1024, 1024),
    )
    .await?;

  println!("Response image bytes: {} | {:?}", response.image.len(), &response.image[..50]);

  // 将生成的图片保存到文件
  let output_path = "runs/generated_image.png";
  vec_to_image_file(&response.image, output_path)
    .map_err(|e| AiError::Custom(format!("Failed to save image: {}", e)))?;

  println!("图片已生成并保存到: {}", output_path);

  Ok(())
}
