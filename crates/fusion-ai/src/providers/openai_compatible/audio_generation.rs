use crate::providers::openai_compatible::Client;
use bytes::Bytes;
use rig::audio_generation::{self, AudioGenerationError, AudioGenerationRequest, AudioGenerationResponse};
use rig::http_client::{self, HttpClientExt};
use serde_json::json;

// ================================================================
// OpenAI Audio Generation API
// ================================================================

// 复用 rig 的常量
pub use rig::providers::openai::audio_generation::{TTS_1, TTS_1_HD};

#[derive(Clone)]
pub struct AudioGenerationModel<T = reqwest::Client> {
  client: Client<T>,
  pub model: String,
}

impl<T> AudioGenerationModel<T> {
  pub fn new(client: Client<T>, model: &str) -> Self {
    Self { client, model: model.to_string() }
  }
}

impl<T> audio_generation::AudioGenerationModel for AudioGenerationModel<T>
where
  T: HttpClientExt + Clone + std::fmt::Debug + Default + 'static,
{
  type Response = Bytes;
  type Client = Client<T>;

  fn make(client: &Self::Client, model: impl Into<String>) -> Self {
    Self::new(client.clone(), &model.into())
  }

  async fn audio_generation(
    &self,
    request: AudioGenerationRequest,
  ) -> Result<AudioGenerationResponse<Self::Response>, AudioGenerationError> {
    let body = serde_json::to_vec(&json!({
        "model": self.model,
        "input": request.text,
        "voice": request.voice,
        "speed": request.speed,
    }))?;

    let req = self
      .client
      .post("/audio/speech")?
      .header("Content-Type", "application/json")
      .body(body)
      .map_err(http_client::Error::from)?;

    let response = self.client.send(req).await?;

    if !response.status().is_success() {
      let status = response.status();
      let bytes: Bytes = response.into_body().await?;
      let text = String::from_utf8_lossy(&bytes);

      return Err(AudioGenerationError::ProviderError(format!("{}: {}", status, text)));
    }

    let bytes: Bytes = response.into_body().await?;

    Ok(AudioGenerationResponse { audio: bytes.to_vec(), response: bytes })
  }
}
