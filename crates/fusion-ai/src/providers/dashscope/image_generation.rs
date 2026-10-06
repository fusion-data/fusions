//! 阿里云 DashScope **原生异步任务**图像生成方言（qwen-image 族）。
//!
//! 协议（两步）：
//! 1. 提交：`POST /api/v1/services/aigc/multimodal-generation/generation` 加
//!    `X-DashScope-Async: enable` 头——同步生成耗时长（qwen-image 系实测
//!    50–110s/张，官方建议客户端超时起步 600s），异步径 task_id 可续查、
//!    可中断、进度粒度真实（PENDING/RUNNING/SUCCEEDED/FAILED/CANCELED）；
//!    响应 `{output: {task_id, task_status: "PENDING"}, request_id}`。
//! 2. 轮询：`GET /api/v1/tasks/{task_id}`——成功载荷
//!    `{output: {task_status: "SUCCEEDED", choices: [{message: {content: [{image: "https://…"}]}}]}, usage}`；
//!    失败载荷 `{code, message}`（HTTP 200 + 业务错误码形态）。
//!
//! 客户端形状对齐 [`crate::video_generation::VideoGenerationProvider`]
//! （`submit` + `check_status` 两方法）——同为「提交-轮询」异步方言族。
//! 下载回传不在本层（调用方自行流式落盘——本仓河图创作 M18 消费面：
//! 64MB 上限 + sha256 边写边算 + 30s 单请求超时，URL 官方 24h 时效完成即落盘）。
//!
//! 域名形态迁移备案：旧全局 `dashscope.aliyuncs.com` → 新
//! `{WorkspaceId}.maas.aliyuncs.com`——经 [`Self::with_base_url`] 覆盖单点；
//! region 与 key MUST 同域（官方口径）。
//!
//! 参考：<https://help.aliyun.com/zh/model-studio/qwen-image-api>

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 默认模型（qwen-image 族旗舰档；同族 standard 档无速度优势实证——
/// 河图创作 M18-D1 裁决）。常量取值随家族 /models 实测口径维护。
pub const DEFAULT_MODEL_QWEN_IMAGE: &str = "qwen-image-3.0-pro";

/// 异步任务提交开关头。
const ASYNC_HEADER: (&str, &str) = ("X-DashScope-Async", "enable");

/// 多模态生成端点路径（qwen-image 文生图共用 host 形态）。
const GENERATION_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

/// 任务查询端点路径。
const TASKS_PATH: &str = "/api/v1/tasks/";

/// 默认单请求超时（提交/轮询单次请求——非总任务时长）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 图像生成错误分类（鉴权/参数/限流/上游内部/协议）。
#[derive(Debug, thiserror::Error)]
pub enum DashScopeImageError {
  /// 鉴权失败（401 / InvalidApiKey 等——key 失效或缺失，可重试口径 = 换 key）。
  #[error("鉴权失败：{0}")]
  Auth(String),
  /// 请求参数非法（InvalidParameter 等）。
  #[error("参数非法：{0}")]
  InvalidParameter(String),
  /// 限流（Throttening——退避重试可恢复）。
  #[error("限流：{0}")]
  Throttling(String),
  /// 上游内部错误（InternalError / 5xx）。
  #[error("上游内部错误：{0}")]
  Internal(String),
  /// 任务级失败（轮询终态 FAILED——上游业务失败，code+message）。
  #[error("任务失败：{code} {message}")]
  TaskFailed { code: String, message: String },
  /// 响应协议不合法（缺 task_id / 载荷形态不识别）。
  #[error("响应不合法：{0}")]
  Protocol(String),
  /// 传输层错误（网络/超时）。
  #[error("请求失败：{0}")]
  Transport(String),
}

impl DashScopeImageError {
  /// 鉴权类错误（调用方引导配置 key——河图创作 M18「无 key/失效显式引导」）。
  pub fn is_auth(&self) -> bool {
    matches!(self, Self::Auth(_))
  }
}

/// 异步任务状态机（上游 task_status 全集 + 未知兜底）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashScopeTaskStatus {
  Pending,
  Running,
  Succeeded,
  Failed,
  Canceled,
  /// 上游新状态值（向前兼容——调用方按非终态处理并设总超时兜底）。
  Unknown,
}

impl DashScopeTaskStatus {
  fn parse(raw: &str) -> Self {
    match raw {
      "PENDING" => Self::Pending,
      "RUNNING" => Self::Running,
      "SUCCEEDED" => Self::Succeeded,
      "FAILED" => Self::Failed,
      "CANCELED" | "CANCELLED" => Self::Canceled,
      _ => Self::Unknown,
    }
  }

  /// 终态（SUCCEEDED/FAILED/CANCELED）。
  pub fn is_terminal(&self) -> bool {
    matches!(self, Self::Succeeded | Self::Failed | Self::Canceled)
  }
}

/// 轮询快照（任务状态 + 产物位 + 计量位 + 失败分类）。
#[derive(Debug, Clone)]
pub struct DashScopeTaskSnapshot {
  pub status: DashScopeTaskStatus,
  /// 首个产物 URL（仅 SUCCEEDED 态设；24h 时效——调用方完成即落盘）。
  pub image_url: Option<String>,
  /// 计量载荷原样透传（usage 字段——成本核算后置，字段保留）。
  pub usage: Option<Value>,
  /// 任务级失败分类（仅 FAILED 态设：上游 code + message）。
  pub failure: Option<(String, String)>,
}

/// 图像生成请求（qwen-image 族参数面）。
#[derive(Debug, Clone, Serialize)]
pub struct DashScopeImageRequest {
  pub model: String,
  pub input: DashScopeImageInput,
  pub parameters: DashScopeImageParameters,
}

/// 输入段。
#[derive(Debug, Clone, Serialize)]
pub struct DashScopeImageInput {
  pub prompt: String,
}

/// 参数段（size 三档 + auto、n 固定 1、watermark 显式关——
/// 河图创作 M18 参数治理口径；enable_thinking/prompt_extend 保持官方默认）。
#[derive(Debug, Clone, Serialize)]
pub struct DashScopeImageParameters {
  pub size: Option<String>,
  pub n: Option<u32>,
  pub watermark: Option<bool>,
  pub prompt_extend: Option<bool>,
}

impl DashScopeImageRequest {
  /// 最小请求（默认模型 + prompt）。
  pub fn new(prompt: impl Into<String>) -> Self {
    Self {
      model: DEFAULT_MODEL_QWEN_IMAGE.into(),
      input: DashScopeImageInput { prompt: prompt.into() },
      parameters: DashScopeImageParameters {
        size: None,
        n: Some(1),
        watermark: Some(false),
        prompt_extend: Some(true),
      },
    }
  }

  pub fn with_model(mut self, model: impl Into<String>) -> Self {
    self.model = model.into();
    self
  }

  pub fn with_size(mut self, size: impl Into<String>) -> Self {
    self.parameters.size = Some(size.into());
    self
  }
}

/// 提交响应 wire 形态。
#[derive(Debug, Deserialize)]
struct SubmitResponse {
  #[serde(default)]
  output: SubmitOutput,
  #[serde(default)]
  #[allow(dead_code)]
  request_id: Option<String>,
  #[serde(default)]
  code: Option<String>,
  #[serde(default)]
  message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SubmitOutput {
  #[serde(default)]
  task_id: Option<String>,
  #[serde(default)]
  task_status: Option<String>,
}

/// 轮询响应 wire 形态（成功 = choices[].message.content[].image；
/// 容错兼容 text2image 形态 results[].url）。
#[derive(Debug, Deserialize)]
struct TaskResponse {
  #[serde(default)]
  output: TaskOutput,
  #[serde(default)]
  usage: Option<Value>,
  #[serde(default)]
  code: Option<String>,
  #[serde(default)]
  message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TaskOutput {
  #[serde(default)]
  task_status: Option<String>,
  #[serde(default)]
  choices: Vec<TaskChoice>,
  #[serde(default)]
  results: Vec<TaskResult>,
}

#[derive(Debug, Default, Deserialize)]
struct TaskChoice {
  #[serde(default)]
  message: TaskMessage,
}

#[derive(Debug, Default, Deserialize)]
struct TaskMessage {
  #[serde(default)]
  content: Vec<TaskContent>,
}

#[derive(Debug, Default, Deserialize)]
struct TaskContent {
  #[serde(default)]
  image: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TaskResult {
  #[serde(default)]
  url: Option<String>,
}

/// DashScope 异步图像生成客户端（submit + check_status）。
#[derive(Debug, Clone)]
pub struct DashScopeImageGeneration {
  api_key: String,
  base_url: String,
  timeout: Duration,
  http: reqwest::Client,
}

impl DashScopeImageGeneration {
  pub fn new(api_key: impl Into<String>) -> Self {
    Self {
      api_key: api_key.into(),
      base_url: "https://dashscope.aliyuncs.com".into(),
      timeout: DEFAULT_TIMEOUT,
      http: reqwest::Client::new(),
    }
  }

  /// 覆盖 API host 根（新域名 `{WorkspaceId}.maas.aliyuncs.com` / 测试 mock 注入）。
  pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
    self.base_url = base_url.into().trim_end_matches('/').to_string();
    self
  }

  /// 单请求超时覆盖（提交/轮询单次——总任务时长由调用方轮询循环控制）。
  pub fn with_timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }

  fn auth_error(&self) -> DashScopeImageError {
    DashScopeImageError::Auth(format!("Bearer 鉴权失败（api_key 缺失/失效）"))
  }

  /// 提交异步任务 → `task_id`（响应 task_status 恒 PENDING；非 PENDING 视为协议异常）。
  pub async fn submit(&self, req: &DashScopeImageRequest) -> Result<String, DashScopeImageError> {
    let url = format!("{}{}", self.base_url, GENERATION_PATH);
    let resp = self
      .http
      .post(&url)
      .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
      .header(CONTENT_TYPE, "application/json")
      .header(ASYNC_HEADER.0, ASYNC_HEADER.1)
      .timeout(self.timeout)
      .json(req)
      .send()
      .await
      .map_err(|e| DashScopeImageError::Transport(e.to_string()))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| DashScopeImageError::Protocol(format!("响应体非 JSON：{e}")))?;
    if status.as_u16() == 401 {
      return Err(DashScopeImageError::Auth(format!(
        "HTTP 401：{}",
        body.get("message").and_then(|v| v.as_str()).unwrap_or("鉴权失败")
      )));
    }
    if !status.is_success() {
      return Err(classify_http_error(status.as_u16(), &body));
    }
    let parsed: SubmitResponse =
      serde_json::from_value(body).map_err(|e| DashScopeImageError::Protocol(format!("提交响应不识别：{e}")))?;
    if let (Some(code), Some(message)) = (parsed.code, parsed.message) {
      // HTTP 200 + 业务错误码形态（InvalidApiKey 等）
      return Err(classify_business_error(&code, &message));
    }
    // 提交响应 task_status 恒 PENDING（异步径契约）——异常态视为协议错误
    let status = parsed.output.task_status.unwrap_or_default();
    if !status.is_empty() && status != "PENDING" {
      return Err(DashScopeImageError::Protocol(format!("提交响应 task_status 非 PENDING：{status}")));
    }
    let task_id = parsed.output.task_id.filter(|id| !id.is_empty());
    match task_id {
      Some(task_id) => Ok(task_id),
      None => Err(DashScopeImageError::Protocol("提交响应缺 output.task_id".into())),
    }
  }

  /// 查询任务快照（终态判定 + 产物 URL 提取 + 失败分类）。
  pub async fn check_status(&self, task_id: &str) -> Result<DashScopeTaskSnapshot, DashScopeImageError> {
    let url = format!("{}{}{}", self.base_url, TASKS_PATH, task_id);
    let resp = self
      .http
      .get(&url)
      .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
      .timeout(self.timeout)
      .send()
      .await
      .map_err(|e| DashScopeImageError::Transport(e.to_string()))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| DashScopeImageError::Protocol(format!("响应体非 JSON：{e}")))?;
    if status.as_u16() == 401 {
      return Err(self.auth_error());
    }
    if !status.is_success() {
      return Err(classify_http_error(status.as_u16(), &body));
    }
    let parsed: TaskResponse =
      serde_json::from_value(body).map_err(|e| DashScopeImageError::Protocol(format!("任务响应不识别：{e}")))?;
    let business_error = parsed
      .code
      .as_deref()
      .zip(parsed.message.as_deref())
      .map(|(code, message)| (code.to_string(), message.to_string()));
    let raw_status = parsed.output.task_status.unwrap_or_default();
    // 无 task_status 的业务错误载荷（任务不存在 / 鉴权失效等）= 查询失败；
    // 有 task_status 的 FAILED 终态 = 任务失败（快照承载，调用方收口）
    if raw_status.is_empty() {
      let (code, message) =
        business_error.ok_or_else(|| DashScopeImageError::Protocol("任务响应缺 output.task_status".into()))?;
      return Err(classify_business_error(&code, &message));
    }
    let task_status = DashScopeTaskStatus::parse(&raw_status);
    let mut snapshot =
      DashScopeTaskSnapshot { status: task_status, image_url: None, usage: parsed.usage, failure: None };
    match task_status {
      DashScopeTaskStatus::Succeeded => {
        // 双形态兼容：multimodal（choices[].message.content[].image）优先，
        // text2image（results[].url）兜底
        let image = parsed
          .output
          .choices
          .iter()
          .flat_map(|c| c.message.content.iter())
          .find_map(|c| c.image.clone().filter(|u| !u.is_empty()))
          .or_else(|| parsed.output.results.iter().find_map(|r| r.url.clone().filter(|u| !u.is_empty())));
        snapshot.image_url =
          Some(image.ok_or_else(|| DashScopeImageError::Protocol("SUCCEEDED 载荷缺产物 URL".into()))?);
      }
      DashScopeTaskStatus::Failed => {
        snapshot.failure = Some(
          business_error.unwrap_or_else(|| ("InternalError".into(), "上游任务失败（载荷未携带 code/message）".into())),
        );
      }
      _ => {}
    }
    Ok(snapshot)
  }
}

/// HTTP 非 2xx 错误分类。
fn classify_http_error(status: u16, body: &Value) -> DashScopeImageError {
  let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("未知错误").to_string();
  let code = body.get("code").and_then(|v| v.as_str()).unwrap_or_default().to_string();
  match status {
    400 => classify_business_error(&code, &message),
    401 => DashScopeImageError::Auth(format!("HTTP 401：{message}")),
    429 => DashScopeImageError::Throttling(format!("HTTP 429：{message}")),
    500..=599 => DashScopeImageError::Internal(format!("HTTP {status}：{message}")),
    _ => DashScopeImageError::Internal(format!("HTTP {status}：{message}")),
  }
}

/// 业务错误码分类（HTTP 200 + code/message / HTTP 400 形态）。
fn classify_business_error(code: &str, message: &str) -> DashScopeImageError {
  if code.is_empty() {
    return DashScopeImageError::Protocol(format!("错误载荷缺 code：{message}"));
  }
  match code {
    "InvalidApiKey" | "Unauthorized" | "AccessDenied" => DashScopeImageError::Auth(format!("{code}：{message}")),
    "InvalidParameter" | "InvalidInput" | "ModelNotFound" | "Arrearage" => {
      DashScopeImageError::InvalidParameter(format!("{code}：{message}"))
    }
    "Throttling" | "Throttling.RateQuota" | "Throttling.AllocationQuota" => {
      DashScopeImageError::Throttling(format!("{code}：{message}"))
    }
    "InternalError" | "ServiceUnavailable" => DashScopeImageError::Internal(format!("{code}：{message}")),
    _ => DashScopeImageError::Internal(format!("{code}：{message}")),
  }
}
