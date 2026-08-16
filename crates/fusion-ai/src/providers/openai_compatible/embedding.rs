//! OpenAI Embedding API（类型本地化）。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::providers::openai_compatible::client::{ApiResponse, Client};
use crate::providers::openai_compatible::errors::OpenAiCompatError;

pub const TEXT_EMBEDDING_3_LARGE: &str = "text-embedding-3-large";
pub const TEXT_EMBEDDING_3_SMALL: &str = "text-embedding-3-small";
pub const TEXT_EMBEDDING_ADA_002: &str = "text-embedding-ada-002";

/// 单条 embedding：文档与向量配对返回。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
  pub document: String,
  pub vec: Vec<f64>,
}

/// Embedding wire 响应（OpenAI 方言）。
#[derive(Debug, Deserialize)]
pub struct EmbeddingResponse {
  pub object: String,
  pub data: Vec<EmbeddingData>,
  pub model: String,
  pub usage: EmbeddingUsage,
}

#[derive(Debug, Deserialize)]
pub struct EmbeddingData {
  pub object: String,
  pub embedding: Vec<serde_json::Number>,
  pub index: usize,
}

#[derive(Debug, Deserialize)]
pub struct EmbeddingUsage {
  #[serde(default)]
  pub prompt_tokens: u64,
  #[serde(default)]
  pub total_tokens: u64,
}

#[derive(Clone)]
pub struct EmbeddingModel {
  client: Client,
  pub model: String,
  ndims: usize,
}

impl EmbeddingModel {
  pub(crate) fn new(client: Client, model: &str, ndims: usize) -> Self {
    Self { client, model: model.to_string(), ndims }
  }

  /// 该模型声明的维度（0 = 未指定）。
  pub fn ndims(&self) -> usize {
    self.ndims
  }

  /// 批量文本向量化。响应条数必须与输入一致，否则视为响应方言异常。
  pub async fn embed_texts(
    &self,
    documents: impl IntoIterator<Item = String>,
  ) -> Result<Vec<Embedding>, OpenAiCompatError> {
    let documents = documents.into_iter().collect::<Vec<_>>();

    let mut body = json!({
        "model": self.model,
        "input": documents,
    });

    if self.ndims > 0 && self.model != TEXT_EMBEDDING_ADA_002 {
      body["dimensions"] = json!(self.ndims);
    }

    let body = serde_json::to_vec(&body).map_err(OpenAiCompatError::from)?;

    let response = self.client.post_json("/embeddings", body).send().await?;
    if !response.status().is_success() {
      return Err(Client::error_from_response(response).await);
    }

    let bytes = response.bytes().await.map_err(|e| OpenAiCompatError::Transport(e.to_string()))?;
    let parsed: ApiResponse<EmbeddingResponse> = serde_json::from_slice(&bytes).map_err(OpenAiCompatError::from)?;

    match parsed {
      ApiResponse::Ok(response) => {
        tracing::info!(target: "fusion_ai",
            "OpenAI embedding token usage: {:?}",
            response.usage
        );

        if response.data.len() != documents.len() {
          return Err(OpenAiCompatError::ResponseParse("Response data length does not match input length".into()));
        }

        Ok(
          response
            .data
            .into_iter()
            .zip(documents)
            .map(|(embedding, document)| Embedding {
              document,
              vec: embedding.embedding.into_iter().map(|n| n.as_f64().unwrap_or(0.0)).collect(),
            })
            .collect(),
        )
      }
      ApiResponse::Err(err) => Err(err.into()),
    }
  }
}
