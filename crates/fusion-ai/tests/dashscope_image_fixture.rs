//! DashScope 异步图像方言 fixture（qwen-image 族——「后加端点先加方言样例」
//! 基线；消费方 = 河图创作 M18 文生图链）。
//!
//! 覆盖：
//! - 提交：`X-DashScope-Async: enable` 头 + `{output:{task_id, task_status:PENDING}, request_id}`；
//! - 轮询三态：PENDING → RUNNING → SUCCEEDED（成功载荷
//!   `output.choices[].message.content[].image` + `usage` 透传）；
//! - text2image 形态兼容（`output.results[].url`——双形态兜底）；
//! - 失败分类：401 鉴权 / 业务错误码（InvalidApiKey / Throttling）/ 任务终态 FAILED；
//! - 提交参数面：n=1 / watermark=false / prompt_extend=true（河图创作 M18 参数治理）。
//!
//! 注：OpenAI 兼容径的 `b64_json` 既有样例见 `multimodal_fixture.rs`——
//! **非 qwen-image 本批路由**（同步长连接不可靠，官方建议超时起步 600s）。
use fusion_ai::providers::dashscope::image_generation::{
  DEFAULT_MODEL_QWEN_IMAGE, DashScopeImageGeneration, DashScopeImageRequest, DashScopeTaskStatus,
};
use serde_json::json;
use wiremock::{
  Mock, MockServer, ResponseTemplate,
  matchers::{method, path},
};

fn client(server: &MockServer) -> DashScopeImageGeneration {
  DashScopeImageGeneration::new("sk-test-dashscope").with_base_url(server.uri())
}

#[tokio::test]
async fn submit_should_post_async_header_and_return_task_id() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/api/v1/services/aigc/multimodal-generation/generation"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "output": { "task_id": "task-7f3a", "task_status": "PENDING" },
      "request_id": "req-1"
    })))
    .mount(&server)
    .await;

  let task_id = client(&server).submit(&DashScopeImageRequest::new("月光下的一只猫")).await.unwrap();
  assert_eq!(task_id, "task-7f3a");

  // 请求体方言：异步开关头 + Bearer + 参数面（n=1 / watermark=false / prompt_extend=true）
  let requests = server.received_requests().await.expect("request recorded");
  let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
  assert_eq!(body["model"], DEFAULT_MODEL_QWEN_IMAGE);
  assert_eq!(body["input"]["prompt"], "月光下的一只猫");
  assert_eq!(body["parameters"]["n"], 1);
  assert_eq!(body["parameters"]["watermark"], false);
  assert_eq!(body["parameters"]["prompt_extend"], true);
}

#[tokio::test]
async fn check_status_should_walk_pending_running_succeeded() {
  let server = MockServer::start().await;
  let c = client(&server);
  // 三态各用独立 task_id 挂样例（避免依赖 wiremock 多 mock 匹配序）
  Mock::given(method("GET"))
    .and(path("/api/v1/tasks/task-pending"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "output": { "task_id": "task-pending", "task_status": "PENDING" }, "request_id": "r1"
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/api/v1/tasks/task-running"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "output": { "task_id": "task-running", "task_status": "RUNNING" }, "request_id": "r2"
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/api/v1/tasks/task-done"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "output": {
        "task_id": "task-done", "task_status": "SUCCEEDED",
        "choices": [{ "finish_reason": "stop", "message": { "content": [{ "image": "https://oss.example/img.png" }] } }]
      },
      "usage": { "image_count": 1 },
      "request_id": "r3"
    })))
    .mount(&server)
    .await;

  let s1 = c.check_status("task-pending").await.unwrap();
  assert_eq!(s1.status, DashScopeTaskStatus::Pending);
  assert!(!s1.status.is_terminal());
  let s2 = c.check_status("task-running").await.unwrap();
  assert_eq!(s2.status, DashScopeTaskStatus::Running);
  let s3 = c.check_status("task-done").await.unwrap();
  assert_eq!(s3.status, DashScopeTaskStatus::Succeeded);
  assert!(s3.status.is_terminal());
  assert_eq!(s3.image_url.as_deref(), Some("https://oss.example/img.png"));
  // usage 原样透传（计量后置——字段保留供成本核算）
  assert_eq!(s3.usage.as_ref().unwrap()["image_count"], 1);
}

#[tokio::test]
async fn check_status_should_accept_text2image_results_form() {
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/api/v1/tasks/task-t2i"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "output": { "task_id": "task-t2i", "task_status": "SUCCEEDED",
        "results": [{ "url": "https://oss.example/t2i.png" }] },
      "usage": { "image_count": 1 }
    })))
    .mount(&server)
    .await;
  let snapshot = client(&server).check_status("task-t2i").await.unwrap();
  assert_eq!(snapshot.status, DashScopeTaskStatus::Succeeded);
  assert_eq!(snapshot.image_url.as_deref(), Some("https://oss.example/t2i.png"));
}

#[tokio::test]
async fn submit_should_classify_401_and_business_auth_error() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/api/v1/services/aigc/multimodal-generation/generation"))
    .respond_with(ResponseTemplate::new(401).set_body_json(json!({
      "code": "InvalidApiKey", "message": "Invalid API-key provided"
    })))
    .mount(&server)
    .await;
  let err = client(&server).submit(&DashScopeImageRequest::new("x")).await.unwrap_err();
  assert!(err.is_auth(), "401 应分类为鉴权失败：{err}");
}

#[tokio::test]
async fn check_status_should_classify_business_error_payload() {
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/api/v1/tasks/task-gone"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "code": "InvalidApiKey", "message": "Invalid API-key provided", "request_id": "r"
    })))
    .mount(&server)
    .await;
  let err = client(&server).check_status("task-gone").await.unwrap_err();
  assert!(err.is_auth(), "InvalidApiKey 业务码应分类为鉴权失败：{err}");
}

#[tokio::test]
async fn check_status_should_surface_failed_terminal_with_code() {
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/api/v1/tasks/task-bad"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "output": { "task_id": "task-bad", "task_status": "FAILED" },
      "code": "InternalError", "message": "content filter rejected",
      "request_id": "r"
    })))
    .mount(&server)
    .await;
  let snapshot = client(&server).check_status("task-bad").await.unwrap();
  assert_eq!(snapshot.status, DashScopeTaskStatus::Failed);
  assert!(snapshot.status.is_terminal());
  assert_eq!(snapshot.failure.as_ref().unwrap().0, "InternalError");
  assert_eq!(snapshot.failure.as_ref().unwrap().1, "content filter rejected");
  assert!(snapshot.image_url.is_none());
}

#[tokio::test]
async fn submit_should_reject_payload_without_task_id() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/api/v1/services/aigc/multimodal-generation/generation"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "request_id": "r" })))
    .mount(&server)
    .await;
  let err = client(&server).submit(&DashScopeImageRequest::new("x")).await.unwrap_err();
  assert!(err.to_string().contains("task_id"), "缺 task_id 应为协议错误：{err}");
}
