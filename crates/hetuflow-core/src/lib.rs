//! hetuflow-core —— 工作流框架核心领域类型与纯计算 helper。
//!
//! 不依赖 hylx-*、ConnectRPC、Axum、SQLx、fusions。只定义领域模型、状态、错误、
//! 纯函数（hash / 退避）与常量。图校验与推进决策在 `hetuflow-runtime`；存储在
//! `hetuflow-sqlx`；HYLX proto↔core 映射、scope、审计、通知在 `hylx-task` adapter。

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
  #[error("validation: {0}")]
  Validation(String),
  #[error("not found: {0}")]
  NotFound(String),
  #[error("conflict: {0}")]
  Conflict(String),
  #[error("storage: {0}")]
  Storage(String),
  #[error("serialization: {0}")]
  Serialization(String),
  #[error("invalid state: {0}")]
  InvalidState(String),
}

pub type Result<T> = std::result::Result<T, FlowError>;

// ---------------------------------------------------------------------------
// 枚举
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
  Pending,
  Active,
  Completed,
  Errored,
  Cancelled,
  /// 通用 rework loop：已退回、等待重做后重入主链的中间态（非终态）。
  ReturnedWaiting,
}

impl WorkflowStatus {
  pub fn is_terminal(&self) -> bool {
    matches!(self, WorkflowStatus::Completed | WorkflowStatus::Errored | WorkflowStatus::Cancelled)
  }

  pub fn try_transition(&self, target: WorkflowStatus) -> Result<WorkflowStatus> {
    match (self, target) {
      (WorkflowStatus::Pending, WorkflowStatus::Active) => Ok(WorkflowStatus::Active),
      (WorkflowStatus::Active, WorkflowStatus::Completed) => Ok(WorkflowStatus::Completed),
      (WorkflowStatus::Active, WorkflowStatus::Errored) => Ok(WorkflowStatus::Errored),
      (WorkflowStatus::Active, WorkflowStatus::Cancelled) => Ok(WorkflowStatus::Cancelled),
      (WorkflowStatus::Completed, WorkflowStatus::Active) => Ok(WorkflowStatus::Active), // resubmitted
      // 通用 rework loop（精确返工）：active→returned_waiting 退回；returned_waiting→active 重入主链；
      // returned_waiting→completed 返工目标被 Rejected 终结。
      (WorkflowStatus::Active, WorkflowStatus::ReturnedWaiting) => Ok(WorkflowStatus::ReturnedWaiting),
      (WorkflowStatus::ReturnedWaiting, WorkflowStatus::Active) => Ok(WorkflowStatus::Active),
      (WorkflowStatus::ReturnedWaiting, WorkflowStatus::Completed) => Ok(WorkflowStatus::Completed),
      _ => Err(FlowError::InvalidState(format!("{self:?} cannot transition to {target:?}"))),
    }
  }

  pub fn as_str(&self) -> &'static str {
    match self {
      WorkflowStatus::Pending => "pending",
      WorkflowStatus::Active => "active",
      WorkflowStatus::Completed => "completed",
      WorkflowStatus::Errored => "errored",
      WorkflowStatus::Cancelled => "cancelled",
      WorkflowStatus::ReturnedWaiting => "returned_waiting",
    }
  }

  pub fn from_db(s: &str) -> WorkflowStatus {
    match s {
      "pending" => WorkflowStatus::Pending,
      "active" => WorkflowStatus::Active,
      "completed" => WorkflowStatus::Completed,
      "cancelled" => WorkflowStatus::Cancelled,
      "returned_waiting" => WorkflowStatus::ReturnedWaiting,
      _ => WorkflowStatus::Errored,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityType {
  Start,
  Approval,
  Notification,
  End,
  Condition,
  Parallel,
  Timer,
  EventWait,
  ParallelSplit,
  ParallelJoin,
  Assignment,
  /// 串行 Merge —— no-op 路由收口节点（穿透推进，不落 activity 行）。
  Merge,
  /// G2 ForEach —— 动态扇出节点（ParallelSplit 动态版，本体不落 activity 行）。
  ForEach,
  /// G3 SubWorkflow（P4.2）—— 子流程嵌套节点；父挂起等子实例终态。
  SubWorkflow,
}

impl ActivityType {
  pub fn as_str(&self) -> &'static str {
    match self {
      ActivityType::Start => "start",
      ActivityType::Approval => "approval",
      ActivityType::Notification => "notification",
      ActivityType::End => "end",
      ActivityType::Condition => "condition",
      ActivityType::Parallel => "parallel",
      ActivityType::Timer => "timer",
      ActivityType::EventWait => "event_wait",
      ActivityType::ParallelSplit => "parallel_split",
      ActivityType::ParallelJoin => "parallel_join",
      ActivityType::Assignment => "assignment",
      ActivityType::Merge => "merge",
      ActivityType::ForEach => "for_each",
      ActivityType::SubWorkflow => "sub_workflow",
    }
  }

  pub fn from_db(s: &str) -> ActivityType {
    match s {
      "start" => ActivityType::Start,
      "approval" => ActivityType::Approval,
      "notification" => ActivityType::Notification,
      "condition" => ActivityType::Condition,
      "parallel" => ActivityType::Parallel,
      "timer" => ActivityType::Timer,
      "event_wait" => ActivityType::EventWait,
      "parallel_split" => ActivityType::ParallelSplit,
      "parallel_join" => ActivityType::ParallelJoin,
      "assignment" => ActivityType::Assignment,
      "merge" => ActivityType::Merge,
      "for_each" => ActivityType::ForEach,
      "sub_workflow" => ActivityType::SubWorkflow,
      _ => ActivityType::End,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
  Pending,
  Active,
  Completed,
  Skipped,
  Errored,
}

impl ActivityStatus {
  pub fn as_str(&self) -> &'static str {
    match self {
      ActivityStatus::Pending => "pending",
      ActivityStatus::Active => "active",
      ActivityStatus::Completed => "completed",
      ActivityStatus::Skipped => "skipped",
      ActivityStatus::Errored => "errored",
    }
  }

  pub fn from_db(s: &str) -> ActivityStatus {
    match s {
      "pending" => ActivityStatus::Pending,
      "active" => ActivityStatus::Active,
      "completed" => ActivityStatus::Completed,
      "skipped" => ActivityStatus::Skipped,
      _ => ActivityStatus::Errored,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
  Approved,
  Returned,
  Rejected,
  Resubmitted,
  /// Phase F1：外部事件 / 回调 signal，推进 EventWait 节点。SignalWorkflow 调用方
  /// 必须附 `event_type` 与当前节点 config 匹配。
  EventReceived,
  Claimed,
  Reassigned,
  Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowResult {
  Approved,
  Rejected,
  Returned,
}

/// Phase G1.5：ParallelJoin 策略（决定何时推进 Join 下游）。
///
/// - `WaitAll`：所有 incoming APPROVED 分支均完成才推进（G1 默认；proto UNSPECIFIED 兼容值）
/// - `WaitAny`：首条分支完成即推进，其它兄弟分支同事务 SKIPPED + 取消 pending timer
/// - `NOfM(n)`：累计 ≥ n 条分支完成即推进；n 必须 ≥ 1 且 ≤ incoming 分支数（validate 强制）
///
/// Rejected/Returned 信号仍走 fail-fast（沿用 G1，与策略正交）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinStrategy {
  WaitAll,
  WaitAny,
  NOfM(i32),
}

impl JoinStrategy {
  /// 给定已到达分支数 `arrived` 与 incoming 总数 `expected`，判定 Join 是否满足。
  pub fn is_satisfied(&self, arrived: i64, expected: i64) -> bool {
    match self {
      JoinStrategy::WaitAll => arrived >= expected,
      JoinStrategy::WaitAny => arrived >= 1,
      JoinStrategy::NOfM(n) => arrived >= *n as i64,
    }
  }

  /// 满足后是否需要 skip 其它仍 active 的兄弟分支 + 取消其 pending timer。
  /// WaitAll 不需要（其它分支理论上已全完成）；WaitAny/N-of-M 需要。
  pub fn skips_remaining_on_satisfaction(&self) -> bool {
    !matches!(self, JoinStrategy::WaitAll)
  }
}

impl WorkflowResult {
  pub fn as_str(&self) -> &'static str {
    match self {
      WorkflowResult::Approved => "approved",
      WorkflowResult::Rejected => "rejected",
      WorkflowResult::Returned => "returned",
    }
  }

  pub fn from_db(s: &str) -> Option<WorkflowResult> {
    match s {
      "approved" => Some(WorkflowResult::Approved),
      "rejected" => Some(WorkflowResult::Rejected),
      "returned" => Some(WorkflowResult::Returned),
      _ => None,
    }
  }
}

// ---------------------------------------------------------------------------
// 图模型（plain；adapter 负责 proto↔core 映射）
// ---------------------------------------------------------------------------

/// 节点 typed 配置（plain enum；替代 proto oneof）。
///
/// Phase E DAG：
/// - `Condition`：分支节点，`default_target` 必填（X3 #10 强制 default），`branches`
///   按顺序评估 CEL guard，首条命中即跳转，全不命中走 default。
/// - `End { result_code }`：多终态支持，`result_code` 决定 workflow 终态结果
///   （"approved" / "rejected" / "returned"；缺省时按触达此 End 的信号默认）。
///
/// Phase F1 运营型节点：
/// - `EventWait`：等待外部事件 / 回调；`timeout_target` 缺省时超时即 ERRORED。
///
/// Phase G1 高级编排：
/// - `ParallelSplit { branch_targets }`：并行分叉，调度 N 个分支活动（branch_targets ≥ 2）。
/// - `ParallelJoin`：并行汇合（wait_all v1；N-of-M / wait_any 留 G1.5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeConfig {
  Start,
  Approval {
    assignee_role: Option<String>,
    sla_seconds: Option<i32>,
    /// Phase F2：审批升级秒数（与 escalation_target_role 配套；validate 强制 > sla_seconds）
    escalation_seconds: Option<i32>,
    /// Phase F2：升级后切换的审批角色 code（与 escalation_seconds 配套）
    escalation_target_role: Option<String>,
  },
  Notification(NotificationConfig),
  End {
    result_code: Option<String>,
  },
  Condition {
    default_target: String,
    branches: Vec<ConditionBranch>,
  },
  Parallel,
  Timer,
  EventWait {
    event_type: String,
    timeout_seconds: Option<i32>,
    timeout_target: Option<String>,
    correlation_key: Option<String>,
    source: Option<EventSourceMetadata>,
  },
  ParallelSplit {
    branch_targets: Vec<String>,
  },
  /// Phase G1.5：ParallelJoin 带 strategy（WAIT_ALL/WAIT_ANY/N_OF_M）
  ParallelJoin {
    strategy: JoinStrategy,
  },
  Assignment(AssignmentConfig),
  /// 串行 Merge —— no-op 路由收口节点。无 typed 字段，下一跳由唯一 APPROVED 出边决定。
  Merge,
  /// G2 ForEach —— 动态扇出节点。`items_path`（CEL，求值 context 得 JSON 数组）+
  /// `branch_template`（单一分支模板节点 id）+ `join_target`（ParallelJoin 汇合点）+
  /// 可选 `max_fanout`（防失控扇出硬上限；None/<=0 = 不限）。
  ForEach {
    items_path: String,
    branch_template: String,
    join_target: String,
    max_fanout: Option<i32>,
  },
  /// G3 SubWorkflow（P4.2）—— 子流程嵌套节点。`child_flow_type` = 子 definition 的 flow_type；
  /// `context_path`（CEL，可选）= 父 context 派生子 context 输入；`on_child_failed_target`（可选）
  /// = 子 errored 时父跳转目标。设计真相源 gaps §6.3.3。
  SubWorkflow {
    child_flow_type: String,
    context_path: Option<String>,
    on_child_failed_target: Option<String>,
  },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NotificationConfig {
  pub template_code: Option<String>,
  pub recipient_selector: Option<RecipientSelector>,
  pub channel_policy: NotificationChannelPolicy,
  pub template_args: std::collections::BTreeMap<String, String>,
  pub reference_type: Option<String>,
  pub purpose: Option<String>,
  pub visibility: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NotificationChannelPolicy {
  pub notification_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RecipientSelector {
  Role { role_code: String, facility_id: Option<String> },
  UserIds { user_ids: Vec<String> },
  BusinessOwner { reference_type: String, reference_id: String, owner_kind: Option<String> },
  CustomResolver { resolver_code: String, args: std::collections::BTreeMap<String, String> },
}

impl RecipientSelector {
  pub fn role_code(&self) -> Option<&str> {
    match self {
      RecipientSelector::Role { role_code, .. } => Some(role_code.as_str()),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSourceMetadata {
  pub source_system: Option<String>,
  pub tenant_id: Option<String>,
  pub facility_id: Option<String>,
  pub reference_type: Option<String>,
  pub reference_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentConfig {
  pub assignee_selector: Option<RecipientSelector>,
  pub queue_code: Option<String>,
  pub sla_seconds: Option<i32>,
}

/// Condition 节点的单条分支（顺序评估，首条 guard true 即跳转）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionBranch {
  /// CEL guard 表达式，根变量 `context` 绑 workflow.context JSON。
  pub guard_expression: String,
  pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
  Start,
  Approval,
  Notification,
  End,
  Condition,
  Parallel,
  Timer,
  EventWait,
  ParallelSplit,
  ParallelJoin,
  Assignment,
  Merge,
  ForEach,
  SubWorkflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNode {
  pub id: String,
  pub name: String,
  #[serde(default)]
  pub context_writes: Vec<String>,
  pub config: NodeConfig,
}

impl WorkflowNode {
  pub fn kind(&self) -> NodeKind {
    match &self.config {
      NodeConfig::Start => NodeKind::Start,
      NodeConfig::Approval { .. } => NodeKind::Approval,
      NodeConfig::Notification(_) => NodeKind::Notification,
      NodeConfig::End { .. } => NodeKind::End,
      NodeConfig::Condition { .. } => NodeKind::Condition,
      NodeConfig::Parallel => NodeKind::Parallel,
      NodeConfig::Timer => NodeKind::Timer,
      NodeConfig::EventWait { .. } => NodeKind::EventWait,
      NodeConfig::ParallelSplit { .. } => NodeKind::ParallelSplit,
      NodeConfig::ParallelJoin { .. } => NodeKind::ParallelJoin,
      NodeConfig::Assignment(_) => NodeKind::Assignment,
      NodeConfig::Merge => NodeKind::Merge,
      NodeConfig::ForEach { .. } => NodeKind::ForEach,
      NodeConfig::SubWorkflow { .. } => NodeKind::SubWorkflow,
    }
  }

  pub fn context_writes(&self) -> &[String] {
    self.context_writes.as_slice()
  }

  /// 审批人角色（仅 APPROVAL 节点有）。
  pub fn assignee_role(&self) -> Option<&str> {
    match &self.config {
      NodeConfig::Approval { assignee_role, .. } => assignee_role.as_deref(),
      _ => None,
    }
  }

  /// 审批 SLA 秒（仅 APPROVAL 节点且 > 0）。
  pub fn sla_seconds(&self) -> Option<i32> {
    match &self.config {
      NodeConfig::Approval { sla_seconds, .. } => sla_seconds.filter(|&s| s > 0),
      _ => None,
    }
  }

  /// Phase F2：审批升级配置（仅 APPROVAL 节点 + 两字段同时设置 + escalation_seconds > 0 时返回）。
  /// validate 阶段已强制 escalation_seconds > sla_seconds + role 非空，故此处不重复校验。
  pub fn escalation_config(&self) -> Option<(i32, &str)> {
    match &self.config {
      NodeConfig::Approval { escalation_seconds, escalation_target_role, .. } => {
        let secs = (*escalation_seconds).filter(|&s| s > 0)?;
        let role = escalation_target_role.as_deref().filter(|s| !s.is_empty())?;
        Some((secs, role))
      }
      _ => None,
    }
  }

  /// Phase F2：返回 APPROVAL 节点的原始 escalation 字段（用于 validate 区分"部分配置" vs "未配置"）。
  pub fn escalation_raw(&self) -> Option<(Option<i32>, Option<&str>)> {
    match &self.config {
      NodeConfig::Approval { escalation_seconds, escalation_target_role, .. } => {
        Some((*escalation_seconds, escalation_target_role.as_deref()))
      }
      _ => None,
    }
  }

  /// End 节点的 result_code（Phase E 多终态；缺省 None 时按触达信号默认）。
  pub fn end_result_code(&self) -> Option<&str> {
    match &self.config {
      NodeConfig::End { result_code } => result_code.as_deref(),
      _ => None,
    }
  }

  /// Condition 节点的 (default_target, branches)；非 Condition 返 None。
  pub fn condition(&self) -> Option<(&str, &[ConditionBranch])> {
    match &self.config {
      NodeConfig::Condition { default_target, branches } => Some((default_target.as_str(), branches.as_slice())),
      _ => None,
    }
  }

  /// EventWait 节点的 (event_type, timeout_seconds, timeout_target)；非 EventWait 返 None。
  /// timeout_seconds 仅在 > 0 时返回 Some；timeout_target 仅在非空 String 时返回 Some。
  pub fn event_wait(&self) -> Option<(&str, Option<i32>, Option<&str>)> {
    match &self.config {
      NodeConfig::EventWait { event_type, timeout_seconds, timeout_target, .. } => Some((
        event_type.as_str(),
        timeout_seconds.filter(|&s| s > 0),
        timeout_target.as_deref().filter(|s| !s.is_empty()),
      )),
      _ => None,
    }
  }

  /// EventWait hardening fields；非 EventWait 返 None。
  pub fn event_wait_correlation(&self) -> Option<(Option<&str>, Option<&EventSourceMetadata>)> {
    match &self.config {
      NodeConfig::EventWait { correlation_key, source, .. } => {
        Some((correlation_key.as_deref().filter(|s| !s.is_empty()), source.as_ref()))
      }
      _ => None,
    }
  }

  pub fn notification(&self) -> Option<&NotificationConfig> {
    match &self.config {
      NodeConfig::Notification(config) => Some(config),
      _ => None,
    }
  }

  pub fn assignment(&self) -> Option<&AssignmentConfig> {
    match &self.config {
      NodeConfig::Assignment(config) => Some(config),
      _ => None,
    }
  }

  /// G3 SubWorkflow（P4.2）节点配置：(child_flow_type, context_path, on_child_failed_target)；
  /// 非 SubWorkflow 返 None。
  pub fn sub_workflow(&self) -> Option<(&str, Option<&str>, Option<&str>)> {
    match &self.config {
      NodeConfig::SubWorkflow { child_flow_type, context_path, on_child_failed_target } => {
        Some((child_flow_type.as_str(), context_path.as_deref(), on_child_failed_target.as_deref()))
      }
      _ => None,
    }
  }

  /// ParallelSplit 节点的 branch_targets 切片；非 ParallelSplit 返 None。
  pub fn parallel_split(&self) -> Option<&[String]> {
    match &self.config {
      NodeConfig::ParallelSplit { branch_targets } => Some(branch_targets.as_slice()),
      _ => None,
    }
  }

  /// 是否 ParallelJoin 节点。
  pub fn is_parallel_join(&self) -> bool {
    matches!(self.config, NodeConfig::ParallelJoin { .. })
  }

  /// Phase G1.5：ParallelJoin 的策略；非 Join 返 None。
  pub fn join_strategy(&self) -> Option<JoinStrategy> {
    match &self.config {
      NodeConfig::ParallelJoin { strategy } => Some(*strategy),
      _ => None,
    }
  }

  /// G2 ForEach 节点的 (items_path, branch_template, join_target, max_fanout)；非 ForEach 返 None。
  /// `max_fanout` 仅在 > 0 时返回 Some。
  pub fn for_each(&self) -> Option<(&str, &str, &str, Option<i32>)> {
    match &self.config {
      NodeConfig::ForEach { items_path, branch_template, join_target, max_fanout } => {
        Some((items_path.as_str(), branch_template.as_str(), join_target.as_str(), max_fanout.filter(|&n| n > 0)))
      }
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTransition {
  pub from: String,
  pub to: String,
  pub on_signal: SignalType,
}

/// definition 元数据 + 图（adapter 从 proto/DB 构造；runtime 只读 nodes/transitions）。
#[derive(Debug, Clone)]
pub struct WorkflowDefinition {
  pub id: String,
  pub tenant_id: String,
  pub facility_id: String,
  pub flow_type: String,
  pub name: String,
  pub version: i32,
  pub nodes: Vec<WorkflowNode>,
  pub transitions: Vec<WorkflowTransition>,
  pub required: bool,
  pub is_active: bool,
  /// 通用 rework loop（缺口 A）：per-definition 返工轮次上限；0 = 未配置，仅受全局
  /// `MAX_RESUBMIT_ROUND` 约束。生效阈值 = min(max_round, 全局硬上限)。
  pub max_round: i32,
  pub created_at: chrono::DateTime<chrono::Utc>,
  pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 运行中实例绑定的不可变 definition 快照（codex #9）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionSnapshot {
  pub flow_type: String,
  pub version: i32,
  pub nodes: Vec<WorkflowNode>,
  pub transitions: Vec<WorkflowTransition>,
  /// 通用 rework loop（缺口 A）：随快照固化的 per-definition 返工轮次上限。
  /// `#[serde(default)]` 保证旧快照（无此字段）反序列化为 0（仅受全局硬上限约束）。
  #[serde(default)]
  pub max_round: i32,
}

// ---------------------------------------------------------------------------
// 实例 / 活动 / outbox / timer 投影模型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WorkflowInstance {
  pub id: String,
  pub tenant_id: String,
  pub facility_id: String,
  pub reference_no: String,
  pub workflow_definition_id: String,
  pub business_key: String,
  pub business_type: String,
  pub status: WorkflowStatus,
  pub current_activity_id: Option<String>,
  pub result: Option<WorkflowResult>,
  pub side_effects_executed: bool,
  pub context: Option<serde_json::Value>,
  pub started_at: Option<chrono::DateTime<chrono::Utc>>,
  pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
  pub created_at: chrono::DateTime<chrono::Utc>,
  pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct ActivityInstance {
  pub id: String,
  pub tenant_id: String,
  pub workflow_instance_id: String,
  pub activity_definition_id: String,
  pub activity_type: ActivityType,
  pub status: ActivityStatus,
  pub assignee_role: Option<String>,
  pub assignee_id: Option<String>,
  pub result: Option<String>,
  pub review_notes: Option<String>,
  pub reviewed_by: Option<String>,
  pub reviewed_at: Option<chrono::DateTime<chrono::Utc>>,
  pub round: i32,
}

#[derive(Debug, Clone)]
pub struct OutboxRecord {
  pub id: String,
  pub tenant_id: String,
  pub facility_id: String,
  pub workflow_instance_id: String,
  pub activity_instance_id: String,
  pub activity_type: String,
  pub payload: serde_json::Value,
  pub status: String,
  pub attempt_count: i32,
  pub max_attempts: i32,
  pub next_attempt_at: chrono::DateTime<chrono::Utc>,
  pub last_error: Option<String>,
  pub created_at: chrono::DateTime<chrono::Utc>,
  pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// durable timer 待触发记录（scanner poll 投影）。
#[derive(Debug, Clone)]
pub struct TimerRecord {
  pub id: String,
  pub tenant_id: String,
  pub facility_id: String,
  pub workflow_instance_id: String,
  pub activity_instance_id: String,
  /// Phase F2：'reminder' | 'escalation'
  pub timer_kind: String,
}

/// event-log 行的只读读模型（projection rebuild / 审计追溯）。
/// `hetuflow_runtime::replay::fold_events` 按 `seq` 升序消费一组 `EventRecord` + 不可变
/// `DefinitionSnapshot` 重建投影；`payload` 是 self-describing 事件体（§8），`created_at`
/// 用于重建 started_at / completed_at / reviewed_at。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
  pub seq: i64,
  pub event_type: String,
  pub payload: serde_json::Value,
  pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 通知 outbox payload。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationPayload {
  pub tenant_id: String,
  pub facility_id: Option<String>,
  pub template_code: String,
  pub recipient_selector: RecipientSelector,
  pub channel_policy: NotificationChannelPolicy,
  pub template_args: std::collections::BTreeMap<String, String>,
  pub reference_type: String,
  pub purpose: String,
  pub visibility: String,
  pub business_type: String,
  pub business_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackErrorKind {
  Deterministic,
  Transient,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackError {
  pub kind: CallbackErrorKind,
  pub message: String,
}

impl CallbackError {
  pub fn deterministic(message: impl Into<String>) -> Self {
    Self { kind: CallbackErrorKind::Deterministic, message: message.into() }
  }

  pub fn transient(message: impl Into<String>) -> Self {
    Self { kind: CallbackErrorKind::Transient, message: message.into() }
  }
}

impl std::fmt::Display for CallbackError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{} ({:?})", self.message, self.kind)
  }
}

impl std::error::Error for CallbackError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackRetryPolicy {
  pub max_attempts: i32,
  pub base_backoff_secs: i64,
  pub cap_backoff_secs: i64,
}

impl Default for CallbackRetryPolicy {
  fn default() -> Self {
    Self { max_attempts: 5, base_backoff_secs: 10, cap_backoff_secs: 300 }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyKeyStrategy {
  PayloadField(String),
  BusinessKey,
  OutboxRecordId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusinessCallbackPayload {
  pub tenant_id: String,
  pub facility_id: String,
  pub workflow_instance_id: String,
  pub activity_instance_id: String,
  pub handler_type: String,
  pub idempotency_key: String,
  pub business_type: String,
  pub business_key: String,
  pub payload: serde_json::Value,
}

pub type CallbackFuture<'a> = Pin<Box<dyn Future<Output = std::result::Result<(), CallbackError>> + Send + 'a>>;

/// Business callback side-effect handler. Object-safe so hylx-task can keep a
/// heterogeneous registry while hetuflow-core remains application-agnostic.
pub trait BusinessCallbackHandler: Send + Sync {
  fn handler_type(&self) -> &str;
  fn execute<'a>(&'a self, payload: &'a BusinessCallbackPayload) -> CallbackFuture<'a>;
  fn retry_policy(&self) -> CallbackRetryPolicy {
    CallbackRetryPolicy::default()
  }
  fn is_reentrant(&self) -> bool {
    true
  }
  fn idempotency_key_strategy(&self) -> IdempotencyKeyStrategy {
    IdempotencyKeyStrategy::PayloadField("idempotency_key".to_string())
  }
  fn error_kind_classify(&self, error: &str) -> CallbackErrorKind {
    let _ = error;
    CallbackErrorKind::Transient
  }
}

// ---------------------------------------------------------------------------
// Scope 过滤（plain；adapter 从 HYLX TaskIdentityScope 转换）
// ---------------------------------------------------------------------------

/// 机构可见性：tenant-scoped（无 facility 限制）或仅可见若干 facility。
#[derive(Debug, Clone)]
pub enum FacilityVisibility {
  /// tenant 内全部可见（不按 facility 过滤）
  AllInTenant,
  /// 仅可见这些 facility（空 = 不可见任何，命中 fail-closed）
  Only(Vec<String>),
}

/// scope-filtered 读的过滤条件（脱 HYLX TaskIdentityScope）。
#[derive(Debug, Clone)]
pub struct ScopeFilter {
  pub tenant_id: String,
  pub facilities: FacilityVisibility,
}

// ---------------------------------------------------------------------------
// 推进决策结果（runtime 产出；service 据此写投影）
// ---------------------------------------------------------------------------

/// Phase G1 并行多目标 —— ParallelSplit / ForEach 调度 N 条分支同时活动。
/// 每条 target 含 node_id + activity_type（分支节点的 kind） + 可选 assignee_role
/// （仅 APPROVAL 分支有）。
///
/// G2 ForEach 扩展：动态分支额外带 `item_index`（数组下标）+ `item_payload`（该元素 JSON），
/// service 记入 `activity.scheduled.v1` 事件 payload。ParallelSplit 分支二者均为 None。
/// 注：含 `serde_json::Value`（不实现 `Eq`），故只 derive `PartialEq`，不 derive `Eq`。
#[derive(Debug, Clone, PartialEq)]
pub struct ParallelTarget {
  pub node_id: String,
  pub activity_type: ActivityType,
  pub assignee_role: Option<String>,
  /// G2 ForEach：该分支对应集合元素的下标（从 0 起）；ParallelSplit 分支为 None。
  pub item_index: Option<i32>,
  /// G2 ForEach：该分支对应集合元素的 JSON 值；ParallelSplit 分支为 None。
  pub item_payload: Option<serde_json::Value>,
}

/// join 计数源（§2.2）。`expected` 来源抽象：ParallelSplit 数静态 incoming 边（`Static`）；
/// ForEach 读 `FOR_EACH_FANNED_OUT` 事件的运行时分支数（`Dynamic`）。两者均按 round 过滤，
/// `JoinStrategy::is_satisfied(arrived, expected)` 逻辑一字不改，只换 `expected` 来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchExpectation {
  /// 现状：数指向 join 的 incoming 边数（ParallelSplit）。
  Static(usize),
  /// ForEach：读 FOR_EACH_FANNED_OUT 事件的 branch_count（运行时定）。
  Dynamic(usize),
}

impl BranchExpectation {
  /// 统一取 expected 计数（供 `JoinStrategy::is_satisfied` 第二参）。
  pub fn count(&self) -> usize {
    match self {
      BranchExpectation::Static(n) | BranchExpectation::Dynamic(n) => *n,
    }
  }
}

// 注：`AdvanceMulti` 内含 `ParallelTarget`（带 `serde_json::Value`，不实现 `Eq`），
// 故只 derive `PartialEq`，不 derive `Eq`。
#[derive(Debug, Clone, PartialEq)]
pub enum AdvanceOutcome {
  /// 推进到下一节点（线性）
  Advance { target_node: String, assignee_role: Option<String> },
  /// Phase G1：并行多目标（ParallelSplit / ForEach 调度 ≥1 分支活动）
  AdvanceMulti { targets: Vec<ParallelTarget> },
  /// 流程到达终态
  Complete { result: WorkflowResult },
  /// 通用 rework loop（精确返工）：Returned 信号命中 RETURNED transition，退回到声明的
  /// ReworkTarget 节点重做（service 据此建活动 + 置 returned_waiting + round+1）。
  Rework { target_node: String, assignee_role: Option<String> },
  /// 找不到合法 transition / 目标节点 —— 错误终态
  Error,
}

// ---------------------------------------------------------------------------
// 事件溯源 / outbox 常量
// ---------------------------------------------------------------------------

// event_type 命名规范（OD-5，2026-06-02）：统一 `namespace.snake_case.v1`（对齐 SDD SPECIFICATION §7.3）。
// 系统未发布、无历史包袱，14 个既有常量一次性加 `.v1`，event-log 历史数据直接重建。
pub mod event_type {
  pub const STARTED: &str = "workflow.started.v1";
  pub const ACTIVITY_SCHEDULED: &str = "activity.scheduled.v1";
  pub const SIGNAL_RECEIVED: &str = "workflow.signal_received.v1";
  pub const ACTIVITY_COMPLETED: &str = "activity.completed.v1";
  pub const ACTIVITY_FAILED: &str = "activity.failed.v1";
  pub const COMPLETED: &str = "workflow.completed.v1";
  pub const ERRORED: &str = "workflow.errored.v1";
  pub const REACTIVATED: &str = "workflow.reactivated.v1";
  /// Phase F1：EventWait timer 到期触发（区别于 SignalReceived 推进路径）。
  pub const TIMER_FIRED: &str = "workflow.timer_fired.v1";
  /// Phase F1：EventWait 活动因 timeout 跳过（区别于 ACTIVITY_COMPLETED）。
  pub const ACTIVITY_SKIPPED: &str = "activity.skipped.v1";
  /// Phase G1：ParallelSplit 调度多分支同事务记录（payload 含 branch_targets 列表）。
  pub const PARALLEL_SPLIT_SCHEDULED: &str = "workflow.parallel_split_scheduled.v1";
  /// Phase G1：分支到达 ParallelJoin（计数）；全部到达后才推进 Join 下游。
  pub const BRANCH_REACHED_JOIN: &str = "workflow.branch_reached_join.v1";
  /// Phase G1：ParallelJoin 所有分支汇合，推进下游。
  pub const PARALLEL_JOIN_SATISFIED: &str = "workflow.parallel_join_satisfied.v1";
  /// Phase F2：审批活动升级（escalation timer fire 后切换 assignee_role）。
  pub const APPROVAL_ESCALATED: &str = "workflow.approval_escalated.v1";
  /// 缺口 B 串行 Merge：穿过 Merge 节点的事实事件（payload 记 from/merge_node/to/round）。
  pub const MERGE_PASSED: &str = "workflow.merge_passed.v1";
  /// 缺口 A 通用 rework loop：精确返工退回到 ReworkTarget（payload 记 from_node/rework_target/round）。
  pub const RETURNED_TO_REWORK: &str = "workflow.returned_to_rework.v1";
  /// 缺口 A 通用 rework loop：ReworkTarget 通过后沿声明路径重入主链（payload 记 rework_target/round）。
  pub const REWORK_RESUMED: &str = "workflow.rework_resumed.v1";
  /// 缺口 D G2 ForEach：动态扇出事实事件（payload 记 join_node/branch_count/round）。
  /// join 计数据此读运行时分支数（BranchExpectation::Dynamic），取代静态 incoming 边数。
  pub const FOR_EACH_FANNED_OUT: &str = "workflow.for_each_fanned_out.v1";
  /// 运行期 context 写回事实事件（payload 记 node/keys/round）。
  pub const CONTEXT_UPDATED: &str = "workflow.context_updated.v1";
  /// G3 SubWorkflow（P4.2）：父 SubWorkflow 节点启动子实例（payload 记 child_instance_id/child_flow_type）。
  pub const SUB_WORKFLOW_STARTED: &str = "workflow.sub_workflow_started.v1";
  /// G3 SubWorkflow（P4.2）：子实例终态回灌父、父 SubWorkflow activity 完成（payload 记 child_instance_id/child_result）。
  pub const SUB_WORKFLOW_COMPLETED: &str = "workflow.sub_workflow_completed.v1";
}

/// G3 SubWorkflow（P4.2）：子实例终态回灌父的内部 business_callback handler 标识 +
/// 回灌 EVENT_RECEIVED 的 event_type。
pub mod sub_workflow {
  pub const CALLBACK_HANDLER_TYPE: &str = "hetuflow.sub_workflow_callback";
  pub const EVENT_TYPE: &str = "hetuflow.sub_workflow.completed";
}

pub mod outbox_activity {
  pub const NOTIFICATION: &str = "notification";
  pub const NOTIFICATION_REMINDER: &str = "notification_reminder";
  /// Phase F2 fix（F-23-4）：escalation 升级后投给新角色的通知；与 NOTIFICATION_REMINDER
  /// 各占 `workflow_activity_outbox` 唯一索引 (instance, activity, activity_type) 位，
  /// 避免 reminder 已 fire 后 escalation 通知被 ON CONFLICT 拦下静默丢失。
  pub const NOTIFICATION_ESCALATION: &str = "notification_escalation";
  pub const BUSINESS_CALLBACK: &str = "business_callback";
}

// ---------------------------------------------------------------------------
// 纯 helper
// ---------------------------------------------------------------------------

/// definition 内容 hash = sha256(canonical json)。canonical = 递归按 key 排序。
pub fn definition_hash(value: &serde_json::Value) -> String {
  use sha2::{Digest, Sha256};
  let mut hasher = Sha256::new();
  hasher.update(canonical_json(value).as_bytes());
  hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn canonical_json(value: &serde_json::Value) -> String {
  match value {
    serde_json::Value::Object(map) => {
      let mut keys: Vec<&String> = map.keys().collect();
      keys.sort();
      let parts: Vec<String> = keys
        .into_iter()
        .map(|k| format!("{}:{}", serde_json::Value::String(k.clone()), canonical_json(&map[k])))
        .collect();
      format!("{{{}}}", parts.join(","))
    }
    serde_json::Value::Array(arr) => {
      let parts: Vec<String> = arr.iter().map(canonical_json).collect();
      format!("[{}]", parts.join(","))
    }
    other => other.to_string(),
  }
}

/// 重试退避（秒）：指数 base*2^attempt，封顶 cap。attempt 从 0 起。
pub fn retry_backoff_secs(attempt: i32, base: i64, cap: i64) -> i64 {
  if attempt < 0 {
    return base;
  }
  let shift = attempt.min(20) as u32;
  let scaled = base.saturating_mul(1i64.checked_shl(shift).unwrap_or(i64::MAX));
  scaled.min(cap)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn status_transitions() {
    assert!(WorkflowStatus::Pending.try_transition(WorkflowStatus::Active).is_ok());
    assert!(WorkflowStatus::Active.try_transition(WorkflowStatus::Completed).is_ok());
    assert!(WorkflowStatus::Completed.try_transition(WorkflowStatus::Active).is_ok());
    assert!(WorkflowStatus::Pending.try_transition(WorkflowStatus::Completed).is_err());
    assert!(WorkflowStatus::Cancelled.try_transition(WorkflowStatus::Active).is_err());
  }

  #[test]
  fn node_kind_and_accessors() {
    let n = WorkflowNode {
      id: "a".into(),
      name: "approve".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some("facility_admin".into()),
        sla_seconds: Some(30),
        escalation_seconds: None,
        escalation_target_role: None,
      },
    };
    assert_eq!(n.kind(), NodeKind::Approval);
    assert_eq!(n.assignee_role(), Some("facility_admin"));
    assert_eq!(n.sla_seconds(), Some(30));
    let s =
      WorkflowNode { id: "s".into(), name: "start".into(), context_writes: Vec::new(), config: NodeConfig::Start };
    assert_eq!(s.kind(), NodeKind::Start);
    assert_eq!(s.assignee_role(), None);
    assert_eq!(s.sla_seconds(), None);
  }

  #[test]
  fn sla_zero_is_none() {
    let n = WorkflowNode {
      id: "a".into(),
      name: "x".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: None,
        sla_seconds: Some(0),
        escalation_seconds: None,
        escalation_target_role: None,
      },
    };
    assert_eq!(n.sla_seconds(), None);
  }

  #[test]
  fn definition_hash_is_stable_and_order_independent() {
    let a = serde_json::json!({"x": 1, "y": [1, 2, {"a": "b"}]});
    let b = serde_json::json!({"y": [1, 2, {"a": "b"}], "x": 1});
    assert_eq!(definition_hash(&a), definition_hash(&b));
    let c = serde_json::json!({"x": 2, "y": [1, 2, {"a": "b"}]});
    assert_ne!(definition_hash(&a), definition_hash(&c));
  }

  #[test]
  fn retry_backoff_is_exponential_then_capped() {
    assert_eq!(retry_backoff_secs(0, 10, 300), 10);
    assert_eq!(retry_backoff_secs(1, 10, 300), 20);
    assert_eq!(retry_backoff_secs(2, 10, 300), 40);
    assert_eq!(retry_backoff_secs(5, 10, 300), 300);
    assert_eq!(retry_backoff_secs(100, 10, 300), 300);
  }

  #[test]
  fn notification_payload_round_trips() {
    let p = NotificationPayload {
      tenant_id: "t".into(),
      facility_id: Some("f".into()),
      template_code: "admission_review_approved".into(),
      recipient_selector: RecipientSelector::Role { role_code: "facility_admin".into(), facility_id: Some("f".into()) },
      channel_policy: NotificationChannelPolicy { notification_types: vec!["NOTIFICATION_TYPE_IN_APP".into()] },
      template_args: std::collections::BTreeMap::new(),
      reference_type: "admission_request".into(),
      purpose: "workflow_notification".into(),
      visibility: "VISIBILITY_ACTION".into(),
      business_type: "admission_request".into(),
      business_key: "bk-1".into(),
    };
    let v = serde_json::to_value(&p).unwrap();
    let back: NotificationPayload = serde_json::from_value(v).unwrap();
    assert!(
      matches!(back.recipient_selector, RecipientSelector::Role { ref role_code, .. } if role_code == "facility_admin")
    );
    assert_eq!(back.business_key, "bk-1");
  }

  // ---- G1.5 JoinStrategy ----

  #[test]
  fn join_strategy_wait_all_only_satisfied_when_all_arrived() {
    assert!(!JoinStrategy::WaitAll.is_satisfied(1, 3));
    assert!(!JoinStrategy::WaitAll.is_satisfied(2, 3));
    assert!(JoinStrategy::WaitAll.is_satisfied(3, 3));
    assert!(JoinStrategy::WaitAll.is_satisfied(4, 3));
    assert!(!JoinStrategy::WaitAll.skips_remaining_on_satisfaction());
  }

  #[test]
  fn join_strategy_wait_any_satisfied_on_first_arrival() {
    assert!(!JoinStrategy::WaitAny.is_satisfied(0, 3));
    assert!(JoinStrategy::WaitAny.is_satisfied(1, 3));
    assert!(JoinStrategy::WaitAny.is_satisfied(3, 3));
    assert!(JoinStrategy::WaitAny.skips_remaining_on_satisfaction());
  }

  #[test]
  fn join_strategy_n_of_m_satisfied_when_threshold_met() {
    let s = JoinStrategy::NOfM(2);
    assert!(!s.is_satisfied(0, 3));
    assert!(!s.is_satisfied(1, 3));
    assert!(s.is_satisfied(2, 3));
    assert!(s.is_satisfied(3, 3));
    assert!(s.skips_remaining_on_satisfaction());
  }

  // ---- F2 escalation_config / escalation_raw ----

  #[test]
  fn escalation_config_returns_some_when_both_set() {
    let n = WorkflowNode {
      id: "a".into(),
      name: "x".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some("facility_admin".into()),
        sla_seconds: Some(30),
        escalation_seconds: Some(120),
        escalation_target_role: Some("facility_director".into()),
      },
    };
    assert_eq!(n.escalation_config(), Some((120, "facility_director")));
  }

  #[test]
  fn escalation_config_none_when_either_unset() {
    let only_secs = WorkflowNode {
      id: "a".into(),
      name: "x".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some("r".into()),
        sla_seconds: None,
        escalation_seconds: Some(120),
        escalation_target_role: None,
      },
    };
    assert_eq!(only_secs.escalation_config(), None);
    let only_role = WorkflowNode {
      id: "a".into(),
      name: "x".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some("r".into()),
        sla_seconds: None,
        escalation_seconds: None,
        escalation_target_role: Some("facility_director".into()),
      },
    };
    assert_eq!(only_role.escalation_config(), None);
    let none = WorkflowNode {
      id: "a".into(),
      name: "x".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some("r".into()),
        sla_seconds: None,
        escalation_seconds: None,
        escalation_target_role: None,
      },
    };
    assert_eq!(none.escalation_config(), None);
  }

  #[test]
  fn escalation_raw_returns_raw_fields_for_validate() {
    let partial = WorkflowNode {
      id: "a".into(),
      name: "x".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some("r".into()),
        sla_seconds: Some(10),
        escalation_seconds: Some(60),
        escalation_target_role: None,
      },
    };
    assert_eq!(partial.escalation_raw(), Some((Some(60), None)));
  }

  #[test]
  fn join_strategy_helper_on_parallel_join_node() {
    let n = WorkflowNode {
      id: "j".into(),
      name: "join".into(),
      context_writes: Vec::new(),
      config: NodeConfig::ParallelJoin { strategy: JoinStrategy::NOfM(2) },
    };
    assert_eq!(n.join_strategy(), Some(JoinStrategy::NOfM(2)));
    assert!(n.is_parallel_join());
  }
}
