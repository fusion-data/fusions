//! 多模态行为基线 fixture（fusion-ai-de-rig.md P0，P3 起走本地 API）。
//!
//! 覆盖 embedding / transcription / image_generation / image_edit /
//! audio_generation 的请求形状（含 multipart）与响应方言解析。

mod fixture_common;

use base64::Engine;
use fixture_common::{API_KEY, request_body};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate};
use wiremock::matchers::{method, path};

use fusion_ai::providers::openai_compatible::audio_generation::AudioGenerationRequest;
use fusion_ai::providers::openai_compatible::image_generation::ImageGenerationRequest;
use fusion_ai::providers::openai_compatible::transcription::TranscriptionRequest;
use fusion_ai::providers::openai_compatible::Client;

async fn mock_client(server: &MockServer) -> Client {
  Client::builder(API_KEY).base_url(server.uri().as_str()).build()
}

// ================================================================
// Embedding
// ================================================================

#[tokio::test]
async fn embedding_request_shape_and_parse() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/embeddings"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "object": "list",
        "data": [
            {"object": "embedding", "embedding": [0.1, 0.2, -0.3], "index": 0},
            {"object": "embedding", "embedding": [0.4, 0.5, -0.6], "index": 1}
        ],
        "model": "text-embedding-3-small",
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    })))
    .expect(1)
    .mount(&server)
    .await;

  let client = mock_client(&server).await;
  let model = client.embedding_model_with_ndims("text-embedding-3-small", 3);
  let embeddings = model
    .embed_texts(vec!["hello".to_string(), "world".to_string()])
    .await
    .expect("embedding succeeds");

  let body = request_body(&server).await;
  assert_eq!(body["model"], "text-embedding-3-small");
  assert_eq!(body["input"], json!(["hello", "world"]));
  // 显式 ndims → dimensions 注入（ada-002 之外）
  assert_eq!(body["dimensions"], 3);

  assert_eq!(embeddings.len(), 2);
  assert_eq!(embeddings[0].document, "hello");
  assert_eq!(embeddings[0].vec, vec![0.1, 0.2, -0.3]);
  assert_eq!(embeddings[1].document, "world");
  assert_eq!(embeddings[1].vec, vec![0.4, 0.5, -0.6]);
}

// ================================================================
// Transcription（multipart）
// ================================================================

#[tokio::test]
async fn transcription_multipart_shape_and_parse() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/audio/transcriptions"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "hello fixture world"})))
    .expect(1)
    .mount(&server)
    .await;

  let client = mock_client(&server).await;
  let model = client.transcription_model("whisper-1");
  let response = model
    .transcription(
      TranscriptionRequest::new(vec![0x11, 0x22, 0x33], "audio.mp3")
        .with_language("zh")
        .with_prompt("context words")
        .with_temperature(0.0),
    )
    .await
    .expect("transcription succeeds");

  let requests = server.received_requests().await.expect("request recorded");
  let request = &requests[0];
  let content_type = request
    .headers
    .get("content-type")
    .expect("content-type present")
    .to_str()
    .unwrap();
  assert!(content_type.starts_with("multipart/form-data"), "unexpected content-type: {content_type}");

  let body = String::from_utf8_lossy(&request.body);
  for expected in [
    r#"name="model""#,
    "whisper-1",
    r#"name="file"; filename="audio.mp3""#,
    r#"name="language""#,
    "zh",
    r#"name="prompt""#,
    r#"name="temperature""#,
  ] {
    assert!(body.contains(expected), "multipart body missing `{expected}`:\n{body}");
  }

  assert_eq!(response.text, "hello fixture world");
}

// ================================================================
// Image Generation
// ================================================================

#[tokio::test]
async fn image_generation_request_shape_and_b64_parse() {
  let server = MockServer::start().await;
  let image_bytes = b"fake-png-bytes".to_vec();
  let b64 = base64::prelude::BASE64_STANDARD.encode(&image_bytes);
  Mock::given(method("POST"))
    .and(path("/images/generations"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "created": 1723600000,
        "data": [{"b64_json": b64}]
    })))
    .expect(1)
    .mount(&server)
    .await;

  let client = mock_client(&server).await;
  let model = client.image_generation_model("dall-e-3");
  let response = model
    .image_generation(ImageGenerationRequest::new("a serene lake at dawn").with_size(1024, 1024))
    .await
    .expect("image generation succeeds");

  let body = request_body(&server).await;
  assert_eq!(body["model"], "dall-e-3");
  assert_eq!(body["prompt"], "a serene lake at dawn");
  assert_eq!(body["size"], "1024x1024");
  // dall-e 系（非 gpt-image-1）强制 b64_json 回传格式
  assert_eq!(body["response_format"], "b64_json");

  assert_eq!(response.image, image_bytes);
}

// ================================================================
// Image Edit（multipart）
// ================================================================

#[tokio::test]
async fn image_edit_multipart_shape_and_b64_parse() {
  let server = MockServer::start().await;
  let image_bytes = b"edited-png-bytes".to_vec();
  let b64 = base64::prelude::BASE64_STANDARD.encode(&image_bytes);
  Mock::given(method("POST"))
    .and(path("/images/edits"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "created": 1723600001,
        "data": [{"b64_json": b64}]
    })))
    .expect(1)
    .mount(&server)
    .await;

  let client = mock_client(&server).await;
  let model = client.image_edit_model("dall-e-2");
  let response = model
    .image_edit(fusion_ai::providers::openai_compatible::image_edit::ImageEditRequest::new_single(
      vec![0xAA, 0xBB],
      "add a red hat".into(),
      "512x512".into(),
    ))
    .await
    .expect("image edit succeeds");

  let requests = server.received_requests().await.expect("request recorded");
  let request = &requests[0];
  let content_type = request.headers.get("content-type").expect("content-type present").to_str().unwrap();
  assert!(content_type.starts_with("multipart/form-data"), "unexpected content-type: {content_type}");

  let body = String::from_utf8_lossy(&request.body);
  for expected in [
    r#"name="model""#,
    "dall-e-2",
    r#"name="prompt""#,
    r#"name="size""#,
    "512x512",
    r#"name="image"; filename="image.png""#,
    r#"name="n""#,
    // dall-e-2 默认 b64_json 回传
    r#"name="response_format""#,
  ] {
    assert!(body.contains(expected), "multipart body missing `{expected}`:\n{body}");
  }

  assert_eq!(response.image, image_bytes);
}

// ================================================================
// Audio Generation（TTS）
// ================================================================

#[tokio::test]
async fn audio_generation_request_shape_and_bytes_parse() {
  let server = MockServer::start().await;
  let audio_bytes: Vec<u8> = vec![0x01, 0x02, 0x03, 0x04];
  Mock::given(method("POST"))
    .and(path("/audio/speech"))
    .respond_with(ResponseTemplate::new(200).set_body_bytes(audio_bytes.clone()))
    .expect(1)
    .mount(&server)
    .await;

  let client = mock_client(&server).await;
  let model = client.audio_generation_model("tts-1");
  let response = model
    .audio_generation(AudioGenerationRequest::new("Today's weather is sunny", "alloy").with_speed(1.0))
    .await
    .expect("audio generation succeeds");

  let body = request_body(&server).await;
  assert_eq!(body["model"], "tts-1");
  assert_eq!(body["input"], "Today's weather is sunny");
  assert_eq!(body["voice"], "alloy");
  assert_eq!(body["speed"], 1.0);

  assert_eq!(response.audio, audio_bytes);
}

// ================================================================
// verify：401 / 5xx 分支（§7 风险表 fixture 覆盖要求）
// ================================================================

#[tokio::test]
async fn verify_classifies_401_and_5xx() {
  // 401 → Http { 401 }（非瞬态）
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/models"))
    .respond_with(ResponseTemplate::new(401))
    .expect(1)
    .mount(&server)
    .await;
  let client = mock_client(&server).await;
  let err = client.verify().await.expect_err("401 must fail");
  match &err {
    fusion_ai::providers::openai_compatible::errors::OpenAiCompatError::Http { status, .. } => assert_eq!(*status, 401),
    other => panic!("expected Http error, got {other:?}"),
  }
  assert!(!err.is_upstream_transient());

  // 5xx → Http { 503 }（瞬态）
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/models"))
    .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
    .expect(1)
    .mount(&server)
    .await;
  let client = mock_client(&server).await;
  let err = client.verify().await.expect_err("503 must fail");
  assert!(err.is_upstream_transient());

  // 200 → Ok
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/models"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": []})))
    .expect(1)
    .mount(&server)
    .await;
  let client = mock_client(&server).await;
  client.verify().await.expect("200 verifies");
}
