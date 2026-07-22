//! # Rig Provider Client Factory
//!
//! This module provides a unified factory for creating LLM provider clients
//! using the explicit provider pattern (recommended for rig 0.27+).
//!
//! ## Example
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use fusion_ai::factory::ClientFactory;
//!
//! let factory = ClientFactory::new();
//! let openai_client = factory.openai("sk-...")?;
//! let deepseek_client = factory.deepseek("sk-...", Some("https://api.deepseek.com"))?;
//! # Ok(())
//! # }
//! ```

use derive_builder::Builder;
use rig::client::{CompletionClient, EmbeddingsClient};
use rig::embeddings::{Embedding, EmbeddingModel};
use rig::{agent::Agent, embeddings, http_client};
use thiserror::Error;

use crate::providers::openai_compatible::{ClientWrapper, ClientWrapperCompletionModel, create_client};

use rig::providers::{
  anthropic, anthropic::completion::CompletionModel as AnthropicCompletionModel, cohere,
  cohere::CompletionModel as CohereCompletionModel, deepseek, deepseek::CompletionModel as DeepSeekCompletionModel,
  gemini, gemini::completion::CompletionModel as GeminiCompletionModel, groq,
  groq::CompletionModel as GroqCompletionModel, huggingface,
  huggingface::completion::CompletionModel as HuggingFaceCompletionModel, mistral,
  mistral::CompletionModel as MistralCompletionModel, ollama, ollama::CompletionModel as OllamaCompletionModel, openai,
  openai::CompletionModel as OpenAICompletionModel, openrouter,
  openrouter::CompletionModel as OpenRouterCompletionModel, perplexity,
  perplexity::CompletionModel as PerplexityCompletionModel, together,
  together::CompletionModel as TogetherCompletionModel, xai, xai::completion::CompletionModel as XAICompletionModel,
};

/// Factory errors
#[derive(Debug, Error)]
pub enum FactoryError {
  #[error("Invalid provider: {0}")]
  InvalidProvider(String),

  #[error("Missing API key for provider: {0}")]
  MissingApiKey(String),

  #[error("Missing base URL for provider: {0}")]
  MissingBaseUrl(String),

  #[error("HTTP client error: {0}")]
  HttpClientError(#[from] http_client::Error),

  #[error("Embedding error: {0}")]
  EmbeddingError(#[from] embeddings::EmbeddingError),
}

/// Unified client factory for creating provider-specific clients.
/// This follows rig 0.27's recommendation of using explicit provider types
/// instead of the deprecated DynClientBuilder pattern.
#[derive(Clone, Default)]
pub struct ClientFactory {}

/// Generate the `*_agent` factory methods whose body is the identical
/// `self._build_agent(config, client.agent(&config.model))` one-liner.
///
/// Every provider whose rig client exposes `.agent(model)` and needs no
/// provider-specific agent wiring is listed here; OpenAI and the
/// OpenAI-compatible wrapper differ enough to stay hand-written.
macro_rules! forward_agent_methods {
  ( $( $(#[$doc:meta])* $name:ident => $client:ty, $model:ty; )* ) => {
    $(
      $(#[$doc])*
      pub fn $name(
        &self,
        config: &AgentConfig,
        client: &$client,
      ) -> Result<Agent<$model>, FactoryError> {
        self._build_agent(config, client.agent(&config.model))
      }
    )*
  };
}

impl ClientFactory {
  /// Create a new client factory
  pub fn new() -> Self {
    Self {}
  }

  /// Create an OpenAI client
  pub fn openai(&self, api_key: &str) -> http_client::Result<openai::Client> {
    openai::Client::new(api_key)
  }

  /// Create an Anthropic client
  pub fn anthropic(&self, api_key: &str) -> http_client::Result<anthropic::Client> {
    anthropic::Client::new(api_key)
  }

  /// Create a DeepSeek client
  pub fn deepseek(&self, api_key: &str, base_url: Option<&str>) -> http_client::Result<deepseek::Client> {
    if let Some(url) = base_url {
      deepseek::Client::builder().api_key(api_key).base_url(url).build()
    } else {
      deepseek::Client::new(api_key)
    }
  }

  /// Create a Groq client
  pub fn groq(&self, api_key: &str) -> http_client::Result<groq::Client> {
    groq::Client::new(api_key)
  }

  /// Create an xAI client
  pub fn xai(&self, api_key: &str, base_url: Option<&str>) -> http_client::Result<xai::Client> {
    if let Some(url) = base_url {
      xai::Client::builder().api_key(api_key).base_url(url).build()
    } else {
      xai::Client::new(api_key)
    }
  }

  /// Create an OpenRouter client
  pub fn openrouter(&self, api_key: &str) -> http_client::Result<openrouter::Client> {
    openrouter::Client::new(api_key)
  }

  /// Create a Google/Gemini client
  pub fn google(&self, api_key: &str) -> http_client::Result<gemini::Client> {
    gemini::Client::new(api_key)
  }

  /// Create a Cohere client
  pub fn cohere(&self, api_key: &str) -> http_client::Result<cohere::Client> {
    cohere::Client::new(api_key)
  }

  /// Create a HuggingFace client
  pub fn huggingface(&self, api_key: &str) -> http_client::Result<huggingface::Client> {
    huggingface::Client::new(api_key)
  }

  /// Create a TogetherAI client
  pub fn togetherai(&self, api_key: &str) -> http_client::Result<together::Client> {
    together::Client::new(api_key)
  }

  /// Create a Perplexity client
  pub fn perplexity(&self, api_key: &str) -> http_client::Result<perplexity::Client> {
    perplexity::Client::new(api_key)
  }

  /// Create a Mistral client
  pub fn mistral(&self, api_key: &str) -> http_client::Result<mistral::Client> {
    mistral::Client::new(api_key)
  }

  /// Create an Ollama client (local)
  pub fn ollama(&self, base_url: &str) -> http_client::Result<ollama::Client> {
    ollama::Client::builder().base_url(base_url).api_key(rig::client::Nothing).build()
  }

  /// Create an OpenAI-compatible client
  pub fn openai_compatible(&self, base_url: &str, api_key: &str) -> ClientWrapper {
    ClientWrapper::new(base_url, api_key)
  }

  /// Create an OpenAI agent
  pub fn openai_agent(
    &self,
    config: &AgentConfig,
    client: &openai::Client,
  ) -> Result<Agent<OpenAICompletionModel>, FactoryError> {
    // completions_api() takes ownership, so we clone the client
    let completions_client = client.clone().completions_api();
    let mut builder = completions_client.agent(&config.model);
    if let Some(system_prompt) = &config.system_prompt {
      builder = builder.preamble(system_prompt);
    }
    for doc in &config.static_context {
      builder = builder.context(doc);
    }
    if let Some(temperature) = config.temperature {
      builder = builder.temperature(temperature);
    }
    if let Some(max_tokens) = config.max_tokens {
      builder = builder.max_tokens(max_tokens);
    }
    if let Some(params) = &config.additional_params {
      builder = builder.additional_params(params.clone());
    }
    Ok(builder.build())
  }

  forward_agent_methods! {
    /// Create an Anthropic agent
    anthropic_agent => anthropic::Client, AnthropicCompletionModel;
    /// Create a DeepSeek agent
    deepseek_agent => deepseek::Client, DeepSeekCompletionModel;
    /// Create a Groq agent
    groq_agent => groq::Client, GroqCompletionModel;
    /// Create an xAI agent
    xai_agent => xai::Client, XAICompletionModel;
    /// Create an OpenRouter agent
    openrouter_agent => openrouter::Client, OpenRouterCompletionModel;
    /// Create a Google/Gemini agent
    google_agent => gemini::Client, GeminiCompletionModel;
    /// Create a Cohere agent
    cohere_agent => cohere::Client, CohereCompletionModel;
    /// Create a HuggingFace agent
    huggingface_agent => huggingface::Client, HuggingFaceCompletionModel;
    /// Create a TogetherAI agent
    togetherai_agent => together::Client, TogetherCompletionModel;
    /// Create a Perplexity agent
    perplexity_agent => perplexity::Client, PerplexityCompletionModel;
    /// Create a Mistral agent
    mistral_agent => mistral::Client, MistralCompletionModel;
    /// Create an Ollama agent
    ollama_agent => ollama::Client, OllamaCompletionModel;
  }

  /// Create an OpenAI-compatible agent
  pub fn openai_compatible_agent(
    &self,
    config: &AgentConfig,
  ) -> Result<Agent<ClientWrapperCompletionModel>, FactoryError> {
    let client = create_client(config)?;
    let mut builder = ClientWrapperCompletionModel::new(client, &config.model).into_agent_builder();
    if let Some(system_prompt) = &config.system_prompt {
      builder = builder.preamble(system_prompt);
    }
    for doc in &config.static_context {
      builder = builder.context(doc);
    }
    if let Some(temperature) = config.temperature {
      builder = builder.temperature(temperature);
    }
    if let Some(max_tokens) = config.max_tokens {
      builder = builder.max_tokens(max_tokens);
    }
    Ok(builder.build())
  }

  /// Internal helper to build an agent with common options
  fn _build_agent<M: rig::completion::CompletionModel>(
    &self,
    config: &AgentConfig,
    mut builder: rig::agent::AgentBuilder<M>,
  ) -> Result<Agent<M>, FactoryError> {
    if let Some(system_prompt) = &config.system_prompt {
      builder = builder.preamble(system_prompt);
    }
    for doc in &config.static_context {
      builder = builder.context(doc);
    }
    if let Some(temperature) = config.temperature {
      builder = builder.temperature(temperature);
    }
    if let Some(max_tokens) = config.max_tokens {
      builder = builder.max_tokens(max_tokens);
    }
    if let Some(params) = &config.additional_params {
      builder = builder.additional_params(params.clone());
    }
    Ok(builder.build())
  }
}

/// Agent configuration for creating agents
#[derive(Clone, Default, Builder)]
pub struct AgentConfig {
  #[builder(setter(into))]
  pub provider: String,

  #[builder(setter(into))]
  pub model: String,

  #[builder(default, setter(into, strip_option))]
  pub base_url: Option<String>,

  #[builder(default, setter(into, strip_option))]
  pub api_key: Option<String>,

  #[builder(default, setter(into, strip_option))]
  pub name: Option<String>,

  #[builder(default, setter(into, strip_option))]
  pub description: Option<String>,

  #[builder(default, setter(into, strip_option))]
  pub system_prompt: Option<String>,

  #[builder(default, setter(into))]
  pub static_context: Vec<String>,

  #[builder(default, setter(strip_option))]
  pub max_tokens: Option<u64>,

  #[builder(default, setter(strip_option))]
  pub temperature: Option<f64>,

  #[builder(default, setter(into, strip_option))]
  pub additional_params: Option<serde_json::Value>,
}

impl std::fmt::Debug for AgentConfig {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AgentConfig")
      .field("provider", &self.provider)
      .field("model", &self.model)
      .field("base_url", &self.base_url)
      .field("api_key", &self.api_key.as_ref().map(|_| "<REDACTED>"))
      .field("name", &self.name)
      .field("description", &self.description)
      .field("system_prompt", &self.system_prompt)
      .field("static_context", &self.static_context)
      .field("max_tokens", &self.max_tokens)
      .field("temperature", &self.temperature)
      .field("additional_params", &self.additional_params)
      .finish()
  }
}

impl AgentConfig {
  /// Create a new agent config
  pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
    Self { provider: provider.into(), model: model.into(), ..Default::default() }
  }
}

impl ClientFactory {
  /// Create embeddings from configuration
  pub async fn embeddings(
    &self,
    config: &EmbeddingConfig,
    documents: Vec<String>,
  ) -> Result<Vec<Embedding>, FactoryError> {
    match config.provider.as_str() {
      "openai" => {
        let api_key = config.api_key.as_deref().ok_or_else(|| FactoryError::MissingApiKey(config.provider.clone()))?;
        let client = self.openai(api_key)?;
        let model = client.embedding_model_with_ndims(&config.model, config.dims);
        Ok(model.embed_texts(documents).await?)
      }
      "google" | "gemini" => {
        let api_key = config.api_key.as_deref().ok_or_else(|| FactoryError::MissingApiKey(config.provider.clone()))?;
        let client = self.google(api_key)?;
        let model = client.embedding_model_with_ndims(&config.model, config.dims);
        Ok(model.embed_texts(documents).await?)
      }
      "cohere" => {
        let api_key = config.api_key.as_deref().ok_or_else(|| FactoryError::MissingApiKey(config.provider.clone()))?;
        let client = self.cohere(api_key)?;
        // Cohere uses embedding_model_with_ndims which takes input_type parameter
        let model = client.embedding_model_with_ndims(&config.model, "search_document", config.dims);
        Ok(model.embed_texts(documents).await?)
      }
      "huggingface" => Err(FactoryError::InvalidProvider("HuggingFace provider no support embeddings".to_string())),
      "together" => {
        let api_key = config.api_key.as_deref().ok_or_else(|| FactoryError::MissingApiKey(config.provider.clone()))?;
        let client = self.togetherai(api_key)?;
        let model = client.embedding_model_with_ndims(&config.model, config.dims);
        Ok(model.embed_texts(documents).await?)
      }
      "mistral" => {
        let api_key = config.api_key.as_deref().ok_or_else(|| FactoryError::MissingApiKey(config.provider.clone()))?;
        let client = self.mistral(api_key)?;
        let model = client.embedding_model_with_ndims(&config.model, config.dims);
        Ok(model.embed_texts(documents).await?)
      }
      "ollama" => {
        let client = self.ollama(config.base_url.as_deref().unwrap_or("http://localhost:11434"))?;
        let model = client.embedding_model_with_ndims(&config.model, config.dims);
        Ok(model.embed_texts(documents).await?)
      }
      "openai-compatible" => {
        let api_key = config.api_key.as_deref().ok_or_else(|| FactoryError::MissingApiKey(config.provider.clone()))?;
        let client = self.openai_compatible(config.base_url.as_deref().unwrap_or(""), api_key);
        let model = client.to_inner().embedding_model_with_ndims(&config.model, config.dims);
        Ok(model.embed_texts(documents).await?)
      }
      _ => Err(FactoryError::InvalidProvider(config.provider.clone())),
    }
  }
}

/// Embedding configuration for creating embedding models
#[derive(Clone, Default, Builder)]
pub struct EmbeddingConfig {
  #[builder(setter(into))]
  pub provider: String,

  #[builder(setter(into))]
  pub model: String,

  pub dims: usize,

  #[builder(default, setter(into, strip_option))]
  pub base_url: Option<String>,

  #[builder(default, setter(into, strip_option))]
  pub api_key: Option<String>,
}

impl std::fmt::Debug for EmbeddingConfig {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("EmbeddingConfig")
      .field("provider", &self.provider)
      .field("model", &self.model)
      .field("dims", &self.dims)
      .field("base_url", &self.base_url)
      .field("api_key", &self.api_key.as_ref().map(|_| "<REDACTED>"))
      .finish()
  }
}

impl EmbeddingConfig {
  /// Create a new embedding config
  pub fn new(provider: impl Into<String>, model: impl Into<String>, dims: usize) -> Self {
    Self { provider: provider.into(), model: model.into(), dims, base_url: None, api_key: None }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn config_debug_never_leaks_api_key() {
    let mut agent = AgentConfig::new("openai", "gpt-x");
    agent.api_key = Some("sk-agent-secret".into());
    let dbg = format!("{agent:?}");
    assert!(!dbg.contains("sk-agent-secret"), "api_key leaked: {dbg}");
    assert!(dbg.contains("REDACTED"));

    let mut embedding = EmbeddingConfig::new("openai", "text-embedding-x", 1536);
    embedding.api_key = Some("sk-embed-secret".into());
    let dbg = format!("{embedding:?}");
    assert!(!dbg.contains("sk-embed-secret"), "api_key leaked: {dbg}");
    assert!(dbg.contains("REDACTED"));
  }
}
