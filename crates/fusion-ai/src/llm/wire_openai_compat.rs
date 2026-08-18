//! OpenAI 兼容 chat/completions wire 实现 —— 给 Qwen / DeepSeek / OpenAI 三家
//! 共享 endpoint / request / response 数据结构。
//!
//! 三家差异：
//! - endpoint URL（base_url / chat/completions 路径不同）
//! - bearer token 字符串（auth header 都用 `Authorization: Bearer <api_key>`）
//! - 默认 model 名（caller 注入）
//! - tool_calls 路径细节（基本一致；个别 vendor 用 `function_call` 旧字段，已 deprecated）

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
  ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ChatRole, LlmError, LlmProviderId, TokenUsage, ToolCall,
  ToolChoice, ToolDefinition,
};

/// 默认 HTTP timeout —— Qwen/DeepSeek/OpenAI 推理 P95 充裕余量。caller 可通过
/// [`OpenAiCompatTransport::with_timeout`] 覆盖。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// OpenAI 兼容 transport —— 三家共享。
///
/// 所有 `with_*` builder 方法统一返回 `Self`（链式不割裂）。底层 reqwest client
/// 延迟到首次 [`chat_complete`](Self::chat_complete) 时构建并**缓存复用**（reqwest
/// 连接池要求单例复用，否则每次请求都要重新 TCP+TLS 握手），构建错误在那时返回。
#[derive(Clone)]
pub struct OpenAiCompatTransport {
  base_url: String,
  api_key: String,
  /// HTTP 请求超时，由 [`with_timeout`](Self::with_timeout) 覆盖。
  timeout: Duration,
  /// 额外 header（OpenAI 兼容场景下用，如 OpenAI `OpenAI-Organization`、Qwen
  /// `X-DashScope-WorkSpace`）；wire 调用时按顺序 set。
  extra_headers: Vec<(String, String)>,
  /// 首次请求时按 `timeout` 构建后缓存；clone 共享同一连接池。
  client: Arc<OnceLock<HttpClient>>,
}

impl fmt::Debug for OpenAiCompatTransport {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("OpenAiCompatTransport")
      .field("base_url", &self.base_url)
      .field("api_key", &"<REDACTED>")
      .field("timeout", &self.timeout)
      .field("extra_headers", &self.extra_headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>())
      .finish_non_exhaustive()
  }
}

impl OpenAiCompatTransport {
  pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self, LlmError> {
    Ok(Self {
      base_url: base_url.into(),
      api_key: api_key.into(),
      timeout: DEFAULT_TIMEOUT,
      extra_headers: Vec::new(),
      client: Arc::new(OnceLock::new()),
    })
  }

  /// 覆盖 HTTP 超时。构建错误（如非法 timeout）推迟到首次请求时返回。
  pub fn with_timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    // timeout 参与 client 构建：丢弃已缓存的 client，下次请求按新 timeout 重建。
    self.client = Arc::new(OnceLock::new());
    self
  }

  pub fn with_extra_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
    self.extra_headers.push((name.into(), value.into()));
    self
  }

  /// 用于测试注入 wiremock-style 替代 endpoint。
  pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
    self.base_url = base_url.into();
    self
  }

  pub fn base_url(&self) -> &str {
    &self.base_url
  }

  /// 取缓存的 reqwest client；首次调用按当前 `timeout` 构建。构建失败映射为
  /// [`LlmError::ConfigInvalid`]。
  fn http(&self) -> Result<&HttpClient, LlmError> {
    if let Some(client) = self.client.get() {
      return Ok(client);
    }
    let built = HttpClient::builder()
      .timeout(self.timeout)
      .build()
      .map_err(|e| LlmError::ConfigInvalid(LlmProviderId::OpenAi, format!("reqwest client build failed: {e}")))?;
    // 并发首次调用时可能重复构建，get_or_init 保证只保留一份。
    Ok(self.client.get_or_init(|| built))
  }

  /// 调 `<base_url>/chat/completions`，把 OpenAI 兼容 wire 响应解析为通用
  /// [`ChatCompletionResponse`]。
  pub async fn chat_complete(
    &self,
    provider: LlmProviderId,
    default_model: &str,
    req: ChatCompletionRequest,
  ) -> Result<ChatCompletionResponse, LlmError> {
    let http = self.http()?;
    let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
    let mut body = build_request_body(default_model, &req);
    maybe_disable_thinking(&mut body, provider, req.tool_choice.as_ref());

    let mut builder = http.post(&url).bearer_auth(&self.api_key).json(&body);
    if let Some(timeout) = req.timeout {
      builder = builder.timeout(timeout);
    }
    for (k, v) in &self.extra_headers {
      builder = builder.header(k, v);
    }

    let resp = builder.send().await.map_err(|e| LlmError::Transport { provider, message: e.to_string() })?;

    let status = resp.status();
    if !status.is_success() {
      let text = resp.text().await.unwrap_or_default();
      return Err(LlmError::Http { provider, status: status.as_u16(), message: truncate(&text, 1024) });
    }

    let parsed: WireChatCompletionResponse =
      resp.json().await.map_err(|e| LlmError::ResponseParse { provider, message: e.to_string() })?;
    parsed.into_response(provider, default_model, &req).ok_or(LlmError::NoChoice { provider })
  }
}

/// 构造 OpenAI 兼容请求 body。
pub fn build_request_body(default_model: &str, req: &ChatCompletionRequest) -> Value {
  let model = req.model.as_deref().unwrap_or(default_model);

  let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
  if let Some(system) = req.system_prompt.as_deref().filter(|s| !s.is_empty()) {
    messages.push(json!({ "role": "system", "content": system }));
  }
  for m in &req.messages {
    let mut entry = json!({ "role": m.role.as_str(), "content": m.content });
    if !m.tool_calls.is_empty() {
      let tcs: Vec<Value> = m
        .tool_calls
        .iter()
        .map(|c| {
          json!({
            "id": c.id.clone().unwrap_or_default(),
            "type": "function",
            "function": { "name": c.name, "arguments": c.arguments },
          })
        })
        .collect();
      if let Some(obj) = entry.as_object_mut() {
        obj.insert("tool_calls".into(), Value::Array(tcs));
      }
    }
    messages.push(entry);
  }

  let mut body = json!({
    "model": model,
    "messages": messages,
  });
  if let Some(t) = req.temperature
    && let Some(obj) = body.as_object_mut()
  {
    obj.insert("temperature".into(), json!(t));
  }
  if !req.tools.is_empty() {
    let tools_json: Vec<Value> = req
      .tools
      .iter()
      .map(|t: &ToolDefinition| {
        let mut function = serde_json::Map::new();
        function.insert("name".into(), Value::String(t.name.clone()));
        if let Some(desc) = &t.description {
          function.insert("description".into(), Value::String(desc.clone()));
        }
        function.insert("parameters".into(), t.parameters.clone());
        json!({ "type": "function", "function": Value::Object(function) })
      })
      .collect();
    if let Some(obj) = body.as_object_mut() {
      obj.insert("tools".into(), Value::Array(tools_json));
    }
  }
  if let Some(choice) = req.tool_choice.as_ref() {
    let v = match choice {
      ToolChoice::Auto => json!("auto"),
      ToolChoice::Required => json!("required"),
      ToolChoice::Function(name) => json!({ "type": "function", "function": { "name": name } }),
    };
    if let Some(obj) = body.as_object_mut() {
      obj.insert("tool_choice".into(), v);
    }
  }
  body
}

/// 思考模式（CoT）vendor 在**强制** `tool_choice`（具名 function / `required`）下拒绝请求，
/// 均以 HTTP 400 `invalid_request_error` 返回，错误文案各异但根因同源 —— 思考模式与强制
/// function calling 不兼容：
/// - DeepSeek V4（`thinking` 默认 enabled）：`"Thinking mode does not support this tool_choice"`
/// - Qwen3（DashScope compatible-mode，`enable_thinking` 默认 true）：
///   `"The tool_choice parameter does not support being set to required or object in thinking mode"`
///
/// 结构化抽取（如 voice NLU 的 `submit_voice_draft`）本就无需 CoT，非思考模式更快更省且兼容强制
/// function calling，故仅当本次调用强制 tool_choice 时，按 vendor 在 body 顶层注入各自的关思考字段：
/// - DeepSeek：`"thinking": {"type": "disabled"}`
/// - Qwen：`"enable_thinking": false`（DashScope 扩展参数）
///
/// `auto`/`none` 未强制 → 不触发不兼容 → 不注入（保留 vendor 思考默认行为）。其它 OpenAI 兼容
/// vendor（OpenAI / Gemini）不识别这些字段，一律不注入，把行为变更收敛到恰好触发不兼容的场景。
fn maybe_disable_thinking(body: &mut Value, provider: LlmProviderId, tool_choice: Option<&ToolChoice>) {
  let forced = matches!(tool_choice, Some(ToolChoice::Function(_)) | Some(ToolChoice::Required));
  if !forced {
    return;
  }
  let Some(obj) = body.as_object_mut() else { return };
  match provider {
    LlmProviderId::DeepSeek => {
      obj.insert("thinking".into(), json!({ "type": "disabled" }));
    }
    LlmProviderId::Qwen => {
      obj.insert("enable_thinking".into(), json!(false));
    }
    _ => {}
  }
}

fn truncate(s: &str, max: usize) -> String {
  if s.len() <= max {
    s.to_string()
  } else {
    // Truncate on a UTF-8 char boundary: `s[..max]` would panic if `max`
    // landed inside a multi-byte code point.
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
  }
}

// ---------------------------------------------------------------------------
// OpenAI 兼容 wire 响应 → 通用 [`ChatCompletionResponse`] 转换
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct WireChatCompletionResponse {
  #[serde(default)]
  id: Option<String>,
  #[serde(default)]
  model: Option<String>,
  #[serde(default)]
  choices: Vec<WireChoice>,
  #[serde(default)]
  usage: Option<WireUsage>,
  /// 透传 vendor extra 字段（system_fingerprint / request_id / 等）
  #[serde(flatten, default)]
  extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
  #[serde(default)]
  finish_reason: Option<String>,
  message: WireMessage,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
  #[serde(default)]
  role: Option<String>,
  #[serde(default)]
  content: Option<String>,
  #[serde(default)]
  tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
  #[serde(default)]
  id: Option<String>,
  #[serde(default, rename = "type")]
  _type: Option<String>,
  function: WireFunctionCall,
}

#[derive(Debug, Deserialize)]
struct WireFunctionCall {
  name: String,
  arguments: String,
}

/// OpenAI 兼容 wire 用量。`cached_input_tokens` 与 `providers::openai_compatible`
/// 的 completion `Usage` 同源 —— 双方言判据单点 =
/// [`crate::providers::openai_compatible::completion::cache_hit_input_tokens`]，
/// 两套 wire MUST NOT 各持方言解析副本。
#[derive(Debug, Clone, Serialize, Default)]
struct WireUsage {
  #[serde(default)]
  prompt_tokens: u32,
  #[serde(default)]
  completion_tokens: u32,
  #[serde(default)]
  total_tokens: u32,
  #[serde(default)]
  cached_input_tokens: u32,
}

impl<'de> Deserialize<'de> for WireUsage {
  /// 先收原始 usage JSON：三方 token 走常规字段，cache 命中经共享判据函数求值
  /// （u64 → u32 饱和，绝不为 absurd 值炸整张响应）。
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Base {
      prompt_tokens: u32,
      completion_tokens: u32,
      total_tokens: u32,
    }
    let raw = serde_json::Value::deserialize(deserializer)?;
    let base: Base = serde_json::from_value(raw.clone()).map_err(serde::de::Error::custom)?;
    let cached = crate::providers::openai_compatible::completion::cache_hit_input_tokens(&raw);
    Ok(Self {
      prompt_tokens: base.prompt_tokens,
      completion_tokens: base.completion_tokens,
      total_tokens: base.total_tokens,
      cached_input_tokens: u32::try_from(cached).unwrap_or(u32::MAX),
    })
  }
}

impl From<WireUsage> for TokenUsage {
  fn from(u: WireUsage) -> Self {
    Self {
      prompt_tokens: u.prompt_tokens,
      completion_tokens: u.completion_tokens,
      total_tokens: u.total_tokens,
      cached_input_tokens: u.cached_input_tokens,
    }
  }
}

impl WireChatCompletionResponse {
  fn into_response(
    self,
    provider: LlmProviderId,
    default_model: &str,
    _req: &ChatCompletionRequest,
  ) -> Option<ChatCompletionResponse> {
    let model = self.model.clone().unwrap_or_else(|| default_model.to_string());
    let usage = self.usage.clone().map(TokenUsage::from);
    let finish_reason = self.choices.first().and_then(|c| c.finish_reason.clone());

    let mut metadata = serde_json::Map::new();
    metadata.insert("provider".into(), Value::String(provider.as_str().to_string()));
    metadata.insert("model".into(), Value::String(model.clone()));
    if let Some(id) = self.id.clone() {
      metadata.insert("response_id".into(), Value::String(id));
    }
    if let Some(fr) = finish_reason {
      metadata.insert("finish_reason".into(), Value::String(fr));
    }
    if let Some(u) = self.usage.as_ref() {
      metadata.insert("usage".into(), serde_json::to_value(u).unwrap_or(Value::Null));
    }
    for (k, v) in self.extra.iter() {
      if k != "choices" && k != "usage" && k != "model" && k != "id" {
        metadata.insert(k.clone(), v.clone());
      }
    }

    let choice = self.choices.into_iter().next()?;
    let WireMessage { role, content, tool_calls } = choice.message;
    let role_parsed = role
      .as_deref()
      .map(|s| match s {
        "system" => ChatRole::System,
        "user" => ChatRole::User,
        "tool" => ChatRole::Tool,
        _ => ChatRole::Assistant,
      })
      .unwrap_or(ChatRole::Assistant);

    let tool_calls: Vec<ToolCall> = tool_calls
      .into_iter()
      .map(|t| ToolCall { id: t.id, name: t.function.name, arguments: t.function.arguments })
      .collect();

    Some(ChatCompletionResponse {
      model,
      message: ChatMessage { role: role_parsed, content: content.unwrap_or_default(), tool_calls },
      usage,
      provider_metadata: Value::Object(metadata),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample_req() -> ChatCompletionRequest {
    ChatCompletionRequest {
      model: Some("custom-model".into()),
      system_prompt: Some("you are helpful".into()),
      messages: vec![ChatMessage::user("hi")],
      tools: vec![ToolDefinition {
        name: "submit".into(),
        description: Some("submit a payload".into()),
        parameters: json!({"type":"object","properties":{}}),
      }],
      tool_choice: Some(ToolChoice::Function("submit".into())),
      temperature: Some(0.0),
      timeout: None,
    }
  }

  #[test]
  fn build_request_body_includes_system_user_tools() {
    let body = build_request_body("default", &sample_req());
    assert_eq!(body["model"], "custom-model");
    assert_eq!(body["temperature"], 0.0);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "you are helpful");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "hi");
    assert_eq!(body["tools"][0]["function"]["name"], "submit");
    assert_eq!(body["tool_choice"]["function"]["name"], "submit");
  }

  #[test]
  fn build_request_body_falls_back_to_default_model() {
    let mut req = sample_req();
    req.model = None;
    let body = build_request_body("fallback-model", &req);
    assert_eq!(body["model"], "fallback-model");
  }

  #[test]
  fn build_request_body_omits_system_when_empty() {
    let mut req = sample_req();
    req.system_prompt = None;
    let body = build_request_body("m", &req);
    // 第一条不应是 system
    assert_ne!(body["messages"][0]["role"], "system");
  }

  #[test]
  fn build_request_body_omits_tool_choice_when_none() {
    let mut req = sample_req();
    req.tool_choice = None;
    let body = build_request_body("m", &req);
    assert!(body.get("tool_choice").is_none());
  }

  #[test]
  fn build_request_body_tool_choice_required() {
    let mut req = sample_req();
    req.tool_choice = Some(ToolChoice::Required);
    let body = build_request_body("m", &req);
    assert_eq!(body["tool_choice"], "required");
  }

  #[test]
  fn deepseek_forced_function_disables_thinking() {
    // 回归：DeepSeek V4 思考模式拒绝强制 tool_choice（HTTP 400）；强制具名函数时须关 thinking。
    let mut body = build_request_body("deepseek-v4-flash", &sample_req());
    maybe_disable_thinking(&mut body, LlmProviderId::DeepSeek, sample_req().tool_choice.as_ref());
    assert_eq!(body["thinking"]["type"], "disabled");
    // 强制 tool_choice 仍保留（非思考模式下合法）。
    assert_eq!(body["tool_choice"]["function"]["name"], "submit");
  }

  #[test]
  fn deepseek_required_disables_thinking() {
    let mut req = sample_req();
    req.tool_choice = Some(ToolChoice::Required);
    let mut body = build_request_body("deepseek-v4-flash", &req);
    maybe_disable_thinking(&mut body, LlmProviderId::DeepSeek, req.tool_choice.as_ref());
    assert_eq!(body["thinking"]["type"], "disabled");
  }

  #[test]
  fn deepseek_auto_tool_choice_keeps_thinking_default() {
    // auto/none 未强制 → 不触发不兼容 → 不注入 thinking（保留 vendor 默认行为）。
    let mut req = sample_req();
    req.tool_choice = Some(ToolChoice::Auto);
    let mut body = build_request_body("deepseek-v4-flash", &req);
    maybe_disable_thinking(&mut body, LlmProviderId::DeepSeek, req.tool_choice.as_ref());
    assert!(body.get("thinking").is_none());
  }

  #[test]
  fn qwen_forced_function_disables_thinking_via_enable_thinking() {
    // 回归：Qwen3（DashScope compatible-mode）思考模式拒绝强制 tool_choice（HTTP 400）；
    // 强制具名函数时须注入 `enable_thinking: false`（vendor 专用字段，非 DeepSeek 的 thinking）。
    let mut body = build_request_body("qwen3.7-plus", &sample_req());
    maybe_disable_thinking(&mut body, LlmProviderId::Qwen, sample_req().tool_choice.as_ref());
    assert_eq!(body["enable_thinking"], false);
    assert!(body.get("thinking").is_none(), "Qwen 用 enable_thinking，不应注入 DeepSeek 的 thinking");
    // 强制 tool_choice 仍保留（非思考模式下合法）。
    assert_eq!(body["tool_choice"]["function"]["name"], "submit");
  }

  #[test]
  fn qwen_required_disables_thinking() {
    let mut req = sample_req();
    req.tool_choice = Some(ToolChoice::Required);
    let mut body = build_request_body("qwen3.7-plus", &req);
    maybe_disable_thinking(&mut body, LlmProviderId::Qwen, req.tool_choice.as_ref());
    assert_eq!(body["enable_thinking"], false);
  }

  #[test]
  fn qwen_auto_tool_choice_keeps_thinking_default() {
    // auto/none 未强制 → 不触发不兼容 → 不注入 enable_thinking（保留 vendor 默认行为）。
    let mut req = sample_req();
    req.tool_choice = Some(ToolChoice::Auto);
    let mut body = build_request_body("qwen3.7-plus", &req);
    maybe_disable_thinking(&mut body, LlmProviderId::Qwen, req.tool_choice.as_ref());
    assert!(body.get("enable_thinking").is_none());
  }

  #[test]
  fn other_vendors_never_inject_thinking_fields() {
    // OpenAI / Gemini 不识别 thinking / enable_thinking —— 即便强制 tool_choice 也不得注入。
    for provider in [LlmProviderId::OpenAi, LlmProviderId::Gemini] {
      let mut body = build_request_body("m", &sample_req());
      maybe_disable_thinking(&mut body, provider, sample_req().tool_choice.as_ref());
      assert!(body.get("thinking").is_none(), "{provider:?} 不应注入 thinking");
      assert!(body.get("enable_thinking").is_none(), "{provider:?} 不应注入 enable_thinking");
    }
  }

  #[test]
  fn parse_response_extracts_message_and_tool_calls() {
    let raw = r#"{
      "id": "chatcmpl-1",
      "model": "qwen3.7-plus",
      "choices": [
        { "finish_reason": "tool_calls", "message": {
          "role": "assistant", "content": "",
          "tool_calls": [
            { "id": "call_1", "type": "function",
              "function": { "name": "submit", "arguments": "{\"intent\":\"RECORD_VITAL_SIGNS\"}" } }
          ]
        } }
      ],
      "usage": { "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120 }
    }"#;
    let parsed: WireChatCompletionResponse = serde_json::from_str(raw).unwrap();
    let req = ChatCompletionRequest::default();
    let resp = parsed.into_response(LlmProviderId::Qwen, "default-m", &req).expect("must parse");
    assert_eq!(resp.model, "qwen3.7-plus");
    assert_eq!(resp.message.tool_calls.len(), 1);
    assert_eq!(resp.message.tool_calls[0].name, "submit");
    assert!(resp.message.tool_calls[0].arguments.contains("RECORD_VITAL_SIGNS"));
    assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 120);
    assert_eq!(resp.provider_metadata["provider"], "dashscope");
    assert_eq!(resp.provider_metadata["finish_reason"], "tool_calls");
  }

  #[test]
  fn parse_response_uses_default_model_when_missing() {
    let raw = r#"{
      "choices": [ { "message": { "role": "assistant", "content": "hi" } } ]
    }"#;
    let parsed: WireChatCompletionResponse = serde_json::from_str(raw).unwrap();
    let resp = parsed
      .into_response(LlmProviderId::OpenAi, "gpt-4o-mini", &ChatCompletionRequest::default())
      .unwrap();
    assert_eq!(resp.model, "gpt-4o-mini");
    assert_eq!(resp.message.content, "hi");
  }

  #[test]
  fn parse_response_empty_choices_returns_none() {
    let raw = r#"{ "model": "x", "choices": [] }"#;
    let parsed: WireChatCompletionResponse = serde_json::from_str(raw).unwrap();
    assert!(parsed.into_response(LlmProviderId::Qwen, "x", &ChatCompletionRequest::default()).is_none());
  }

  /// helper：从含 usage 的响应 JSON 提取 TokenUsage（走完整 wire 解析路径）。
  fn usage_of(usage_json: &str) -> TokenUsage {
    let raw = format!(
      r#"{{ "model": "m", "choices": [ {{ "message": {{ "role": "assistant", "content": "ok" }} }} ], "usage": {} }}"#,
      usage_json
    );
    let parsed: WireChatCompletionResponse = serde_json::from_str(&raw).unwrap();
    parsed
      .into_response(LlmProviderId::DeepSeek, "m", &ChatCompletionRequest::default())
      .expect("must parse")
      .usage
      .expect("usage present")
  }

  // —— cache 双方言：判据单点 = providers::openai_compatible::completion::cache_hit_input_tokens，
  //    与 openai_compatible wire 对同组输入的断言值一致（跨 wire 一致性另由
  //    tests/usage_cache_dialect_fixture.rs 在 mock server 上钉死）。 ——

  #[test]
  fn parse_usage_deepseek_flat_cache_dialect() {
    let usage = usage_of(
      r#"{ "prompt_tokens": 1000, "completion_tokens": 50, "total_tokens": 1050, "prompt_cache_hit_tokens": 800, "prompt_cache_miss_tokens": 200 }"#,
    );
    assert_eq!(usage.cached_input_tokens, 800);
    assert_eq!(usage.total_tokens, 1050);
  }

  #[test]
  fn parse_usage_openai_nested_cache_dialect() {
    let usage = usage_of(
      r#"{ "prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940, "prompt_tokens_details": { "cached_tokens": 600 } }"#,
    );
    assert_eq!(usage.cached_input_tokens, 600);
  }

  #[test]
  fn parse_usage_both_dialects_present_takes_max() {
    // 防御性：双方言同时出现取 max，避免同 token 双计。
    let usage = usage_of(
      r#"{ "prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940, "prompt_cache_hit_tokens": 100, "prompt_tokens_details": { "cached_tokens": 600 } }"#,
    );
    assert_eq!(usage.cached_input_tokens, 600);
    let flipped = usage_of(
      r#"{ "prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940, "prompt_cache_hit_tokens": 700, "prompt_tokens_details": { "cached_tokens": 600 } }"#,
    );
    assert_eq!(flipped.cached_input_tokens, 700);
  }

  #[test]
  fn parse_usage_without_cache_fields_degrades_to_zero() {
    let usage = usage_of(r#"{ "prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14 }"#);
    assert_eq!(usage.cached_input_tokens, 0, "unknown dialect degrades to 0");
  }

  #[test]
  fn debug_never_leaks_api_key() {
    // 回归：derive(Debug) 曾把 api_key 明文打进日志（tracing::debug!(?transport)）。
    let transport = OpenAiCompatTransport::new("https://api.example.com", "sk-super-secret").unwrap();
    let dbg = format!("{transport:?}");
    assert!(!dbg.contains("sk-super-secret"), "api_key leaked: {dbg}");
    assert!(dbg.contains("<REDACTED>"));
  }

  #[test]
  fn with_timeout_resets_cached_client() {
    let transport = OpenAiCompatTransport::new("https://api.example.com", "k").unwrap();
    let first = transport.http().unwrap() as *const HttpClient;
    let second = transport.http().unwrap() as *const HttpClient;
    assert_eq!(first, second, "client must be built once and reused");
    let transport = transport.with_timeout(Duration::from_secs(3));
    assert!(transport.client.get().is_none(), "timeout change must drop the cached client");
  }
}
