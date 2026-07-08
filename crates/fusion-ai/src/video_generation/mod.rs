//! rig_video_providers
//!
//! Unified VideoGenerationProvider trait with three provider implementations:
//! - openai (feature "openai")
//! - siliconflow (feature "siliconflow")
//! - gitee (feature "gitee")
//!
//! A Router is provided to select which provider to call at runtime.
//!
//! This crate is prepared as a rig-style provider library (compatible with rig patterns).

use ahash::HashMap;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error type for provider operations.
#[derive(Debug, Error)]
pub enum VideoGenerationError {
  #[error("HTTP error: {0}")]
  Http(#[from] reqwest::Error),

  #[error("API error: {0}")]
  Api(String),

  #[error("Serde error: {0}")]
  Serde(#[from] serde_json::Error),

  #[error("Other: {0}")]
  Other(String),
}

/// Request: includes model, prompt, optional duration and a `provider` hint.
/// The `provider` field can be used by Router to pick a provider; if omitted Router uses default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoGenerationRequest {
  pub model: String,
  pub prompt: String,
  pub duration: Option<u32>,
  pub size: Option<String>,
  /// Optional provider hint: "openai", "siliconflow", "gitee"
  pub provider: Option<String>,
  /// Provider-specific options can be put here
  pub options: Option<serde_json::Value>,
}

/// Response from status check.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoGenerationResponse {
  pub ready: bool,
  pub video_url: Option<String>,
  pub meta: Option<serde_json::Value>,
}

/// Provider trait: submit returns a request_id (job id). check_status polls for completion.
#[async_trait]
pub trait VideoGenerationProvider: Send + Sync + 'static {
  async fn submit(&self, req: VideoGenerationRequest) -> Result<String, VideoGenerationError>;
  async fn check_status(&self, request_id: &str) -> Result<VideoGenerationResponse, VideoGenerationError>;
}

/// Separator between the provider-name prefix and the raw provider job id in
/// the composite id returned by [`ProviderRouter::submit`].
///
/// Chosen because provider names registered via [`ProviderRouter::register`]
/// (`"openai"` / `"siliconflow"` / `"gitee"`) never contain `':'`, so the
/// first `':'` unambiguously delimits the prefix.
const ROUTER_ID_SEP: char = ':';

/// Router that holds named providers and delegates calls.
///
/// Because the raw job id returned by each provider's `submit` carries **no
/// provider identity**, [`ProviderRouter::submit`] wraps it in a composite id
/// of the form `"<provider-name>:<raw-id>"`. [`ProviderRouter::check_status`]
/// then parses the prefix and routes directly to the owning provider — no
/// blind try-all fan-out.
pub struct ProviderRouter {
  providers: HashMap<String, Box<dyn VideoGenerationProvider>>,
  default: String,
}

impl ProviderRouter {
  pub fn new(default: impl Into<String>) -> Self {
    Self { providers: HashMap::default(), default: default.into() }
  }

  pub fn register<P: VideoGenerationProvider>(&mut self, name: impl Into<String>, provider: P) {
    self.providers.insert(name.into(), Box::new(provider));
  }

  /// Resolve the provider name to dispatch a `submit` to, honouring the
  /// optional request hint and falling back to the configured default.
  fn select_provider_name(&self, hint: Option<&str>) -> Result<String, VideoGenerationError> {
    if let Some(h) = hint {
      if self.providers.contains_key(h) {
        return Ok(h.to_string());
      }
      return Err(VideoGenerationError::Other(format!("provider hint '{}' not registered", h)));
    }
    if self.providers.contains_key(&self.default) {
      Ok(self.default.clone())
    } else {
      Err(VideoGenerationError::Other(format!("default provider '{}' not registered", self.default)))
    }
  }
}

#[async_trait]
impl VideoGenerationProvider for ProviderRouter {
  /// Submit a job and return a composite id `"<provider-name>:<raw-id>"` so a
  /// later [`Self::check_status`] can route back to the same provider.
  async fn submit(&self, req: VideoGenerationRequest) -> Result<String, VideoGenerationError> {
    let name = self.select_provider_name(req.provider.as_deref())?;
    let provider = self
      .providers
      .get(&name)
      .ok_or_else(|| VideoGenerationError::Other(format!("provider '{}' not registered", name)))?;
    let raw_id = provider.submit(req).await?;
    Ok(format!("{name}{ROUTER_ID_SEP}{raw_id}"))
  }

  /// Parse the `"<provider-name>:<raw-id>"` prefix produced by [`Self::submit`]
  /// and route the status check to the owning provider.
  async fn check_status(&self, request_id: &str) -> Result<VideoGenerationResponse, VideoGenerationError> {
    let (name, raw_id) = request_id.split_once(ROUTER_ID_SEP).ok_or_else(|| {
      VideoGenerationError::Other(format!(
        "request_id '{request_id}' has no '<provider>{ROUTER_ID_SEP}<id>' prefix — \
         it must come from ProviderRouter::submit"
      ))
    })?;
    let provider = self.providers.get(name).ok_or_else(|| {
      VideoGenerationError::Other(format!("provider '{name}' (from request_id prefix) not registered"))
    })?;
    provider.check_status(raw_id).await
  }
}

//
// Provider modules
//
pub mod gitee;
pub mod openai;
pub mod siliconflow;

// ---------------------- Utilities ----------------------
/// Simple helper: poll until ready with exponential backoff up to max_attempts.
pub async fn poll_until_ready<P: VideoGenerationProvider + ?Sized>(
  provider: &P,
  request_id: &str,
  interval_secs: u64,
  max_attempts: usize,
) -> Result<VideoGenerationResponse, VideoGenerationError> {
  let mut attempts = 0usize;
  loop {
    attempts += 1;
    let status = provider.check_status(request_id).await?;
    if status.ready {
      return Ok(status);
    }
    if attempts >= max_attempts {
      return Err(VideoGenerationError::Other(format!("max attempts reached ({})", max_attempts)));
    }
    tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
  }
}
