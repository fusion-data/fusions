//! hetuflow-runtime —— 纯图校验与推进决策。
//!
//! 持久化原子性由 `hetuflow-sqlx` 负责；本 crate 只做确定性计算 + CEL guard 评估。
//!
//! ## Phase E DAG
//! Approved 信号支持「连锁解析」：transition.to 若为 Condition 节点，按 branches 顺序
//! 评估 CEL guard（context = workflow.context JSON），首条 true 即跳转，全不命中走
//! default_target，递归直到落在 Approval（继续推进）或 End（终态）。X3 #10 强制 default。
//!
//! ## Phase F1 EventWait/Callback
//! - `SignalType::EventReceived`：调用方附 `event_type` 必须与当前 EventWait 节点
//!   config 匹配；匹配则按 `on_signal=EVENT_RECEIVED` transition 推进（resolve_target
//!   与 Approved 同链）。
//! - `decide_event_wait_timeout`：timer 到期触发，按节点 `timeout_target` 推进
//!   （缺省时 ERRORED）。resolve_target 已支持 EventWait 作为 target（Advance）。
//!
//! ## Phase G1 ParallelSplit/Join（高级编排）
//! - 进入 `ParallelSplit` 节点 → `AdvanceMulti { targets }`，service 在单事务内 insert
//!   N 个分支活动（每条 branch_target 一个 activity；branch_target 必须是 Approval kind）。
//! - 分支 Approved → resolve_target.to 必是 ParallelJoin（validate 强制）；service 检测
//!   该返 Advance{target=Join}，特殊处理：append BRANCH_REACHED_JOIN，统计已到达分支
//!   数，若 = Join 的 incoming count 则推进 Join 的 APPROVED 出边（resolve_target 链）。
//! - v1：wait_all 策略 + 仅 Approval 分支 + fail-fast（任一 Rejected/Returned → workflow
//!   ERRORED）。N-of-M / Notification 分支 / partial-success 留 G1.5/G2 触发驱动。

use std::collections::HashSet;

use hetuflow_core::{
  ActivityType, AdvanceOutcome, FlowError, NodeKind, ParallelTarget, Result, SignalType, WorkflowNode, WorkflowResult,
  WorkflowTransition,
};

pub mod guard_eval;
pub mod replay;

/// resolve 时最大递归深度（防 Condition 链恶意/失误环路；正常 DAG ≤ 5 层）。
const MAX_RESOLVE_DEPTH: u32 = 8;

/// ParallelJoin 汇合 incoming signal 放宽集合（§6.3.2 OD-7 / G2 ForEach 需改现状②）。
///
/// 现状只认 `APPROVED`（纯 Approval 并行）；放宽为**超集** `{APPROVED, COMPLETED, EVENT_RECEIVED}`：
/// - 纯 Approval 分支（发 APPROVED）仍命中，汇合判定语义不变（向后兼容）；
/// - 额外允许 ForEach 的 Assignment 分支（发 COMPLETED）/ EventWait 分支（发 EVENT_RECEIVED）汇入。
///
/// validate 的「分支须出边指向 Join」与 Join incoming-count、`handle_branch_reach_join`
/// 的分支识别共用此集合，保持口径一致。
pub fn is_join_incoming_signal(sig: SignalType) -> bool {
  matches!(sig, SignalType::Approved | SignalType::Completed | SignalType::EventReceived)
}

/// `resolve_target` 穿透路径回传（缺口 B 串行 Merge + 缺口 A rework 共享，§2.1）。
///
/// B 落地敲定承载位置：`decide_advance` / `decide_event_wait_timeout` 返回
/// `(AdvanceOutcome, ResolveTrace)` 元组；A 接入仅追加 `loops_passed`，不改承载结构。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveTrace {
  /// 缺口 B：穿过的 Merge 节点 id（service 据此在同事务 append `MERGE_PASSED`）。
  pub merges_passed: Vec<String>,
  /// 缺口 A：rework 退回穿透记录（穿过的 Condition 等中间节点 id，预留）。
  pub loops_passed: Vec<String>,
  /// 缺口 D G2 ForEach：本次 `AdvanceMulti` 由穿过的 ForEach 节点动态扇出时填充
  /// `(for_each_node_id, join_target)`（ForEach 节点本身不落 activity，源 activity 是其
  /// 上游节点，故 service 据此识别 ForEach 来源 + 取 join_target，而非检查源 activity kind）。
  pub for_each_fan_out: Option<ForEachFanOut>,
}

/// 缺口 D G2 ForEach 扇出来源（随 `ResolveTrace` 上浮到 service）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForEachFanOut {
  pub for_each_node: String,
  pub join_target: String,
}

/// 按 `SignalType` 匹配 from 节点的下一个 transition。
pub fn find_next_transition<'a>(
  transitions: &'a [WorkflowTransition],
  from_node: &str,
  signal: SignalType,
) -> Option<&'a WorkflowTransition> {
  transitions.iter().find(|t| t.from == from_node && t.on_signal == signal)
}

/// start 决策（纯）：取第一个 APPROVAL 节点作为初始活动。
pub fn decide_start(nodes: &[WorkflowNode]) -> Option<(String, Option<String>)> {
  nodes
    .iter()
    .find(|n| n.kind() == NodeKind::Approval)
    .map(|n| (n.id.clone(), n.assignee_role().map(str::to_string)))
}

/// advance 决策（DAG 版 + F1 EventWait + 缺口 A rework + 缺口 B Merge）。
///
/// 语义：
/// - `Rejected` → 直接终止（`Complete{Rejected}`）
/// - `Returned`（缺口 A 通用 rework loop）：当前节点声明 `RETURNED` transition → 精确返工
///   （`resolve_target` 解析退回目标，可穿透 Condition/Merge），返回 `Rework`；未声明 →
///   整体重提（`Complete{Returned}`，service 走 `advance_resubmit` 从首审重入）
/// - `Approved` → 跟随 APPROVED transition；若 target 是 Condition/Merge 则连锁解析到
///   Approval/EventWait（`Advance`）或 End（`Complete` + result_code）
/// - `EventReceived` → 跟随 EVENT_RECEIVED transition；resolve_target 同上
/// - `Resubmitted` 由 service 单独走 reactivate 路径
///
/// 返回 `(AdvanceOutcome, ResolveTrace)`：trace 携带穿透路径（merges_passed），
/// service 据此在同事务 append `MERGE_PASSED`。
/// `context`：workflow 实例的 context JSON（绑根变量 `context` 供 CEL 表达式读取）。
pub fn decide_advance(
  nodes: &[WorkflowNode],
  transitions: &[WorkflowTransition],
  current_node: &str,
  signal: SignalType,
  context: Option<&serde_json::Value>,
) -> (AdvanceOutcome, ResolveTrace) {
  let mut trace = ResolveTrace::default();
  let outcome = match signal {
    SignalType::Rejected => AdvanceOutcome::Complete { result: WorkflowResult::Rejected },
    SignalType::Returned => match find_next_transition(transitions, current_node, SignalType::Returned) {
      // 精确返工：解析 RETURNED transition 的退回目标（穿透 Condition/Merge 落 ReworkTarget），
      // 把落点的 Advance 转成 Rework（service 据此建活动 + 置 returned_waiting + round+1）。
      Some(t) => {
        let mut visited: HashSet<String> = HashSet::new();
        match resolve_target(nodes, transitions, &t.to, context, &mut visited, &mut trace, 0) {
          AdvanceOutcome::Advance { target_node, assignee_role } => {
            AdvanceOutcome::Rework { target_node, assignee_role }
          }
          // ReworkTarget 解析异常（落 End/Split/环路/深度超限）→ 错误终态
          _ => AdvanceOutcome::Error,
        }
      }
      // 整体重提：未声明 RETURNED transition，沿用现状 Completed(returned) → advance_resubmit
      None => AdvanceOutcome::Complete { result: WorkflowResult::Returned },
    },
    SignalType::Resubmitted | SignalType::Claimed | SignalType::Reassigned => AdvanceOutcome::Error,
    SignalType::Approved | SignalType::EventReceived | SignalType::Completed => {
      match find_next_transition(transitions, current_node, signal) {
        Some(t) => {
          let mut visited: HashSet<String> = HashSet::new();
          resolve_target(nodes, transitions, &t.to, context, &mut visited, &mut trace, 0)
        }
        None => AdvanceOutcome::Error,
      }
    }
  };
  (outcome, trace)
}

/// F1 EventWait timeout 决策：timer 到期触发时计算下一步。
///
/// 语义：
/// - 当前节点必须是 EventWait（其它类型 → Error，不应发生）
/// - 节点有 `timeout_target` → resolve_target 到该节点（同 Approved 链，可解 Condition）
/// - 节点无 `timeout_target` → Error（流程进入 ERRORED 终态；validate 时已警告）
pub fn decide_event_wait_timeout(
  nodes: &[WorkflowNode],
  transitions: &[WorkflowTransition],
  current_node: &str,
  context: Option<&serde_json::Value>,
) -> (AdvanceOutcome, ResolveTrace) {
  let mut trace = ResolveTrace::default();
  let Some(node) = nodes.iter().find(|n| n.id == current_node) else {
    return (AdvanceOutcome::Error, trace);
  };
  let Some((_, _, timeout_target)) = node.event_wait() else {
    return (AdvanceOutcome::Error, trace);
  };
  let Some(target) = timeout_target else {
    return (AdvanceOutcome::Error, trace);
  };
  let mut visited: HashSet<String> = HashSet::new();
  let outcome = resolve_target(nodes, transitions, target, context, &mut visited, &mut trace, 0);
  (outcome, trace)
}

/// 递归解析 target：Approval → Advance；End → Complete；Condition → 评估 branches/default 后递归；
/// Merge（缺口 B）→ 穿透取唯一 APPROVED 出边递归（记录 `trace.merges_passed`）。
fn resolve_target(
  nodes: &[WorkflowNode],
  transitions: &[WorkflowTransition],
  target_id: &str,
  context: Option<&serde_json::Value>,
  visited: &mut HashSet<String>,
  trace: &mut ResolveTrace,
  depth: u32,
) -> AdvanceOutcome {
  if depth > MAX_RESOLVE_DEPTH {
    return AdvanceOutcome::Error;
  }
  if !visited.insert(target_id.to_string()) {
    return AdvanceOutcome::Error; // cycle
  }
  let Some(node) = nodes.iter().find(|n| n.id == target_id) else {
    return AdvanceOutcome::Error;
  };
  match node.kind() {
    NodeKind::Approval => AdvanceOutcome::Advance {
      target_node: target_id.to_string(),
      assignee_role: node.assignee_role().map(str::to_string),
    },
    NodeKind::EventWait => AdvanceOutcome::Advance {
      target_node: target_id.to_string(),
      // EventWait 无人审批；service 据 node kind 知此活动 activity_type=EVENT_WAIT
      assignee_role: None,
    },
    NodeKind::Notification => AdvanceOutcome::Advance { target_node: target_id.to_string(), assignee_role: None },
    NodeKind::Assignment => AdvanceOutcome::Advance { target_node: target_id.to_string(), assignee_role: None },
    // G3 SubWorkflow（P4.2）：父 advance 到 SubWorkflow 节点 → 调度该 activity（service 据
    // kind=SubWorkflow 在同事务启动子实例 + 挂起父），等子终态 EVENT_RECEIVED 回灌再推进下游。
    NodeKind::SubWorkflow => AdvanceOutcome::Advance { target_node: target_id.to_string(), assignee_role: None },
    NodeKind::ParallelJoin => AdvanceOutcome::Advance {
      target_node: target_id.to_string(),
      // service 检测 kind=ParallelJoin 后特殊处理 join 计数（不 insert activity）
      assignee_role: None,
    },
    NodeKind::Merge => {
      // 缺口 B 串行 Merge：no-op 路由穿透 —— 记录穿过的 Merge（service 据 trace append MERGE_PASSED），
      // 取唯一 APPROVED 出边继续 resolve（与 Condition 同机制，共享 visited cycle 检测 + depth 护栏）。
      // validate 已强制恰好一条 APPROVED 出边、不指向 ParallelSplit、并行分支不得汇入。
      trace.merges_passed.push(target_id.to_string());
      let Some(next) = find_next_transition(transitions, target_id, SignalType::Approved) else {
        return AdvanceOutcome::Error;
      };
      resolve_target(nodes, transitions, &next.to, context, visited, trace, depth + 1)
    }
    NodeKind::ParallelSplit => {
      let Some(branch_targets) = node.parallel_split() else {
        return AdvanceOutcome::Error;
      };
      // P4.1 Mixed Parallel：branch_target kind 放宽为 {Approval, Assignment, EventWait, Notification}，
      // activity_type 由 branch 节点 kind 派生（复用 ForEach 派生范式）；嵌套 / 路由节点 validate 已拒绝，
      // 运行时兜底 Error。
      let mut targets = Vec::with_capacity(branch_targets.len());
      for branch_id in branch_targets {
        let Some(branch_node) = nodes.iter().find(|n| n.id == *branch_id) else {
          return AdvanceOutcome::Error;
        };
        let activity_type = match branch_node.kind() {
          NodeKind::Approval => ActivityType::Approval,
          NodeKind::Assignment => ActivityType::Assignment,
          NodeKind::EventWait => ActivityType::EventWait,
          NodeKind::Notification => ActivityType::Notification,
          _ => return AdvanceOutcome::Error,
        };
        targets.push(ParallelTarget {
          node_id: branch_id.clone(),
          activity_type,
          assignee_role: branch_node.assignee_role().map(str::to_string),
          // ParallelSplit 分支无 item 信息（ForEach 专属）
          item_index: None,
          item_payload: None,
        });
      }
      AdvanceOutcome::AdvanceMulti { targets }
    }
    NodeKind::ForEach => {
      // G2 ForEach 动态扇出：CEL 求值 items_path 得 JSON 数组 → 每元素一条分支活动。
      // activity_type 由 branch_template 节点 kind 派生（需改现状①，不写死 Approval）。
      let Some((items_path, branch_template, join_target, max_fanout)) = node.for_each() else {
        return AdvanceOutcome::Error;
      };
      // 记录 ForEach 扇出来源（service 据此识别 ForEach + 取 join_target；ForEach 节点不落 activity，
      // 源 activity 是其上游节点，不能用源 activity kind 判断）。失败路径（Error）不依赖此字段。
      trace.for_each_fan_out =
        Some(ForEachFanOut { for_each_node: target_id.to_string(), join_target: join_target.to_string() });
      // 求值集合（非数组 / 求值失败 → Error，service 走 ERRORED 终态 for_each_items_invalid）
      let Ok(items) = guard_eval::eval_array(items_path, context) else {
        return AdvanceOutcome::Error;
      };
      // 超 max_fanout（若设）→ Error（service 据此回滚拒绝扇出 + 交人工）
      if let Some(cap) = max_fanout
        && items.len() > cap as usize
      {
        return AdvanceOutcome::Error;
      }
      // branch_template 节点：派生分支 activity_type（Approval/Assignment/EventWait）
      let Some(template_node) = nodes.iter().find(|n| n.id == branch_template) else {
        return AdvanceOutcome::Error;
      };
      let branch_activity_type = match template_node.kind() {
        NodeKind::Approval => ActivityType::Approval,
        NodeKind::Assignment => ActivityType::Assignment,
        NodeKind::EventWait => ActivityType::EventWait,
        // 嵌套/非法 branch_template kind（validate 已拒绝；运行时兜底 Error）
        _ => return AdvanceOutcome::Error,
      };
      let branch_role = template_node.assignee_role().map(str::to_string);
      // 空数组合法：targets 为空 → service 视 expected=0 立即满足 join 推进下游（不卡死）。
      let targets = items
        .into_iter()
        .enumerate()
        .map(|(i, payload)| ParallelTarget {
          node_id: branch_template.to_string(),
          activity_type: branch_activity_type,
          assignee_role: branch_role.clone(),
          item_index: Some(i as i32),
          item_payload: Some(payload),
        })
        .collect();
      AdvanceOutcome::AdvanceMulti { targets }
    }
    NodeKind::End => {
      let result = node.end_result_code().and_then(WorkflowResult::from_db).unwrap_or(WorkflowResult::Approved);
      AdvanceOutcome::Complete { result }
    }
    NodeKind::Condition => {
      let Some((default_target, branches)) = node.condition() else {
        return AdvanceOutcome::Error;
      };
      for branch in branches {
        match guard_eval::eval_guard(&branch.guard_expression, context) {
          Ok(true) => return resolve_target(nodes, transitions, &branch.target, context, visited, trace, depth + 1),
          Ok(false) => continue,
          Err(_) => return AdvanceOutcome::Error, // CEL eval failure → engine error
        }
      }
      // 全不命中走 default（X3 #10 必填）
      resolve_target(nodes, transitions, default_target, context, visited, trace, depth + 1)
    }
    NodeKind::Start | NodeKind::Parallel | NodeKind::Timer => AdvanceOutcome::Error,
  }
}

/// 校验 nodes/transitions 是否构成合法 DAG（Phase E：放开 Condition + 多 End）。
///
/// 约束：
/// - 恰好 1 个 Start；至少 1 个 End；不支持 Parallel/Timer（仍延后）
/// - Approval：assignee_role 非空 + 至少一条 APPROVED 出边
/// - Condition：default_target 非空且引用存在节点；每个 branch.target 引用存在节点；
///   每个 guard CEL eager 编译（语法 / 未知变量等错误立刻 surface）
/// - End：result_code 可缺省（缺省按触达信号默认）
/// - 所有 node id 唯一；transitions.from/to 引用存在节点
pub fn validate_definition(nodes: &[WorkflowNode], transitions: &[WorkflowTransition]) -> Result<()> {
  // 节点 id 唯一
  let mut seen_ids = HashSet::new();
  for n in nodes {
    if !seen_ids.insert(n.id.as_str()) {
      return Err(FlowError::Validation(format!("duplicate node id '{}'", n.id)));
    }
  }

  let start_count = nodes.iter().filter(|n| n.kind() == NodeKind::Start).count();
  let end_count = nodes.iter().filter(|n| n.kind() == NodeKind::End).count();
  if start_count != 1 {
    return Err(FlowError::Validation(format!("nodes must contain exactly one start node (found {start_count})")));
  }
  if end_count == 0 {
    return Err(FlowError::Validation("nodes must contain at least one end node".into()));
  }

  // 节点级校验
  for node in nodes {
    if !node.context_writes().is_empty() {
      if !matches!(
        node.kind(),
        NodeKind::Approval | NodeKind::EventWait | NodeKind::Notification | NodeKind::Assignment
      ) {
        return Err(FlowError::Validation(format!(
          "node '{}' context_writes may only be configured on activity nodes",
          node.id
        )));
      }
      let mut context_keys = HashSet::new();
      for key in node.context_writes() {
        if key.is_empty() {
          return Err(FlowError::Validation(format!("node '{}' context_writes must not contain empty keys", node.id)));
        }
        if !context_keys.insert(key.as_str()) {
          return Err(FlowError::Validation(format!(
            "node '{}' context_writes contains duplicate key '{}'",
            node.id, key
          )));
        }
      }
    }
    match node.kind() {
      NodeKind::Parallel | NodeKind::Timer => {
        return Err(FlowError::Validation(format!("v1.0 does not support node kind {:?}", node.kind())));
      }
      NodeKind::Notification => {
        let Some(config) = node.notification() else {
          return Err(FlowError::Validation(format!("notification node '{}' config missing", node.id)));
        };
        if config.template_code.as_deref().unwrap_or("").is_empty() {
          return Err(FlowError::Validation(format!(
            "notification node '{}' template_code must not be empty",
            node.id
          )));
        }
        if config.recipient_selector.is_none() {
          return Err(FlowError::Validation(format!(
            "notification node '{}' recipient_selector must not be empty",
            node.id
          )));
        }
        if !transitions.iter().any(|t| t.from == node.id && t.on_signal == SignalType::Completed) {
          return Err(FlowError::Validation(format!(
            "notification node '{}' must have a COMPLETED outgoing transition",
            node.id
          )));
        }
      }
      NodeKind::Assignment => {
        let Some(config) = node.assignment() else {
          return Err(FlowError::Validation(format!("assignment node '{}' config missing", node.id)));
        };
        if config.assignee_selector.is_none() && config.queue_code.as_deref().unwrap_or("").is_empty() {
          return Err(FlowError::Validation(format!(
            "assignment node '{}' requires assignee_selector or queue_code",
            node.id
          )));
        }
        if !transitions.iter().any(|t| t.from == node.id && t.on_signal == SignalType::Completed) {
          return Err(FlowError::Validation(format!(
            "assignment node '{}' must have a COMPLETED outgoing transition",
            node.id
          )));
        }
      }
      NodeKind::Approval if node.assignee_role().unwrap_or("").is_empty() => {
        return Err(FlowError::Validation(format!("approval node '{}' assignee_role must not be empty", node.id)));
      }
      NodeKind::Approval => {
        // 至少一条 APPROVED 出边（驳回/退回由 service 直接终态，不要求 transition）
        if !transitions.iter().any(|t| t.from == node.id && t.on_signal == SignalType::Approved) {
          return Err(FlowError::Validation(format!(
            "approval node '{}' must have at least one APPROVED outgoing transition",
            node.id
          )));
        }
        // Phase F2：escalation 字段配套校验（两字段同时配置 + escalation_seconds > sla_seconds）
        if let Some((esc_secs_opt, esc_role_opt)) = node.escalation_raw() {
          let esc_secs = esc_secs_opt.filter(|&s| s > 0);
          let esc_role = esc_role_opt.filter(|s| !s.is_empty());
          match (esc_secs, esc_role) {
            (None, None) => {}
            (Some(_), Some(_)) => {
              if let Some(sla) = node.sla_seconds()
                && esc_secs.unwrap() <= sla
              {
                return Err(FlowError::Validation(format!(
                  "approval node '{}' escalation_seconds ({}) must be > sla_seconds ({})",
                  node.id,
                  esc_secs.unwrap(),
                  sla
                )));
              }
            }
            (Some(_), None) => {
              return Err(FlowError::Validation(format!(
                "approval node '{}' escalation_seconds set but escalation_target_role missing",
                node.id
              )));
            }
            (None, Some(_)) => {
              return Err(FlowError::Validation(format!(
                "approval node '{}' escalation_target_role set but escalation_seconds missing",
                node.id
              )));
            }
          }
        }
      }
      NodeKind::Condition => {
        let (default_target, branches) = node.condition().unwrap();
        if default_target.is_empty() {
          return Err(FlowError::Validation(format!(
            "condition node '{}' must have non-empty default_target (X3 #10 mandatory default)",
            node.id
          )));
        }
        if !seen_ids.contains(default_target) {
          return Err(FlowError::Validation(format!(
            "condition node '{}' default_target '{}' references unknown node",
            node.id, default_target
          )));
        }
        for branch in branches {
          if !seen_ids.contains(branch.target.as_str()) {
            return Err(FlowError::Validation(format!(
              "condition node '{}' branch target '{}' references unknown node",
              node.id, branch.target
            )));
          }
          // eager CEL 编译捕获 guard 语法错误
          guard_eval::validate_expression(&branch.guard_expression)
            .map_err(|e| FlowError::Validation(format!("condition node '{}' branch guard invalid: {}", node.id, e)))?;
        }
      }
      NodeKind::EventWait => {
        let (event_type, _timeout_seconds, timeout_target) = node.event_wait().unwrap();
        if event_type.is_empty() {
          return Err(FlowError::Validation(format!("event_wait node '{}' event_type must not be empty", node.id)));
        }
        // timeout_target 若设则必须引用存在节点
        if let Some(target) = timeout_target
          && !seen_ids.contains(target)
        {
          return Err(FlowError::Validation(format!(
            "event_wait node '{}' timeout_target '{}' references unknown node",
            node.id, target
          )));
        }
        // 必须有至少一条 EVENT_RECEIVED 出边 OR 设置 timeout_target
        // （否则流程进入 EventWait 后永久卡死无任何推进路径）
        let has_signal_transition =
          transitions.iter().any(|t| t.from == node.id && t.on_signal == SignalType::EventReceived);
        if !has_signal_transition && timeout_target.is_none() {
          return Err(FlowError::Validation(format!(
            "event_wait node '{}' must have at least one EVENT_RECEIVED outgoing transition or a timeout_target",
            node.id
          )));
        }
      }
      NodeKind::ParallelSplit => {
        let branch_targets = node.parallel_split().unwrap();
        if branch_targets.len() < 2 {
          return Err(FlowError::Validation(format!(
            "parallel_split node '{}' must have >= 2 branch_targets (got {})",
            node.id,
            branch_targets.len()
          )));
        }
        // 每条 branch_target 必须引用存在节点 + kind=Approval（v1 MVP）+ APPROVED 出边
        // 指向同一个 ParallelJoin 节点
        let mut join_id: Option<&str> = None;
        for bt in branch_targets {
          if !seen_ids.contains(bt.as_str()) {
            return Err(FlowError::Validation(format!(
              "parallel_split node '{}' branch_target '{}' references unknown node",
              node.id, bt
            )));
          }
          let Some(branch_node) = nodes.iter().find(|n| n.id == *bt) else {
            return Err(FlowError::Validation(format!(
              "parallel_split node '{}' branch_target '{}' references unknown node",
              node.id, bt
            )));
          };
          // P4.1 Mixed Parallel：branch kind 放宽为 {Approval, Assignment, EventWait, Notification}；
          // 每条分支的「完成信号」按 kind 取，出边 MUST 指向同一 ParallelJoin。嵌套 / 路由节点拒绝。
          let completion_signal = match branch_node.kind() {
            NodeKind::Approval => SignalType::Approved,
            NodeKind::Assignment | NodeKind::Notification => SignalType::Completed,
            NodeKind::EventWait => SignalType::EventReceived,
            other => {
              return Err(FlowError::Validation(format!(
                "parallel_split node '{}' branch_target '{}' must be one of {{APPROVAL, ASSIGNMENT, EVENT_WAIT, NOTIFICATION}} kind; got {:?}",
                node.id, bt, other
              )));
            }
          };
          if !branch_node.context_writes().is_empty() {
            return Err(FlowError::Validation(format!(
              "parallel_split node '{}' branch_target '{}' must not declare context_writes",
              node.id, bt
            )));
          }
          // branch 的「完成信号」出边必须指向同一 Join 节点
          let approved_to = transitions
            .iter()
            .find(|t| t.from == *bt && t.on_signal == completion_signal)
            .map(|t| t.to.as_str());
          let Some(target_to) = approved_to else {
            return Err(FlowError::Validation(format!(
              "parallel_split node '{}' branch '{}' must have a {:?} outgoing transition (to ParallelJoin)",
              node.id, bt, completion_signal
            )));
          };
          // 验证 target_to 是 ParallelJoin
          let target_node = nodes.iter().find(|n| n.id == target_to);
          if !target_node.map(|n| n.kind() == NodeKind::ParallelJoin).unwrap_or(false) {
            return Err(FlowError::Validation(format!(
              "parallel_split node '{}' branch '{}' APPROVED transition must target a ParallelJoin (got '{}')",
              node.id, bt, target_to
            )));
          }
          match join_id {
            None => join_id = Some(target_to),
            Some(existing) if existing != target_to => {
              return Err(FlowError::Validation(format!(
                "parallel_split node '{}' branches must converge to the SAME ParallelJoin (got '{}' and '{}')",
                node.id, existing, target_to
              )));
            }
            _ => {}
          }
        }
      }
      NodeKind::ParallelJoin => {
        // incoming signal 放宽为超集 {APPROVED, COMPLETED, EVENT_RECEIVED}（§6.3.2 OD-7）。
        let incoming_count =
          transitions.iter().filter(|t| t.to == node.id && is_join_incoming_signal(t.on_signal)).count();
        // 该 Join 是否被某 ForEach 节点 join_target 引用：ForEach 静态只有 1 条 branch_template→join
        // 出边，运行时才扇出 N 条动态分支，故 ForEach-fed join 豁免「≥2 incoming」静态要求。
        let is_for_each_join = nodes.iter().any(|n| n.for_each().map(|(_, _, jt, _)| jt == node.id).unwrap_or(false));
        if !is_for_each_join && incoming_count < 2 {
          return Err(FlowError::Validation(format!(
            "parallel_join node '{}' must have >= 2 incoming branch transitions (APPROVED/COMPLETED/EVENT_RECEIVED) (got {})",
            node.id, incoming_count
          )));
        }
        let has_outgoing = transitions.iter().any(|t| t.from == node.id && t.on_signal == SignalType::Approved);
        if !has_outgoing {
          return Err(FlowError::Validation(format!(
            "parallel_join node '{}' must have an APPROVED outgoing transition (to advance after join)",
            node.id
          )));
        }
        // Phase G1.5：N-of-M 边界校验（ForEach-fed join 分支数运行时定，静态边数不为 incoming 上界，
        // 故 ForEach-fed join 跳过 N>incoming 静态校验；仅校验 n>=1）。
        if let Some(hetuflow_core::JoinStrategy::NOfM(n)) = node.join_strategy() {
          if n < 1 {
            return Err(FlowError::Validation(format!(
              "parallel_join node '{}' N_OF_M strategy requires min_completions >= 1 (got {})",
              node.id, n
            )));
          }
          if !is_for_each_join && (n as usize) > incoming_count {
            return Err(FlowError::Validation(format!(
              "parallel_join node '{}' N_OF_M min_completions ({}) exceeds incoming branches ({})",
              node.id, n, incoming_count
            )));
          }
        }
      }
      NodeKind::Merge => {
        // 缺口 B 串行 Merge no-op 路由收口节点校验（§4.4）。
        let outgoing: Vec<&WorkflowTransition> =
          transitions.iter().filter(|t| t.from == node.id && t.on_signal == SignalType::Approved).collect();
        // 恰好一条 APPROVED 出边（多于一条 → 路由歧义；零条 → 死胡同）
        if outgoing.len() != 1 {
          return Err(FlowError::Validation(format!(
            "merge node '{}' must have exactly one APPROVED outgoing transition (got {})",
            node.id,
            outgoing.len()
          )));
        }
        // 出边 to 不得指向 ParallelSplit（拒绝 Merge 直接引爆并行）
        if let Some(to_node) = nodes.iter().find(|n| n.id == outgoing[0].to)
          && to_node.kind() == NodeKind::ParallelSplit
        {
          return Err(FlowError::Validation(format!(
            "merge node '{}' APPROVED outgoing must not target a ParallelSplit (no implicit fan-out after merge)",
            node.id
          )));
        }
        // 并行分支的 APPROVED 出边不得指向此 Merge（并行汇合必须用 ParallelJoin）
        for split in nodes.iter().filter(|n| n.kind() == NodeKind::ParallelSplit) {
          let Some(branch_targets) = split.parallel_split() else { continue };
          for bt in branch_targets {
            if transitions.iter().any(|t| t.from == *bt && t.on_signal == SignalType::Approved && t.to == node.id) {
              return Err(FlowError::Validation(format!(
                "merge node '{}' must not be the convergence of parallel branches (use ParallelJoin); branch '{}' targets it",
                node.id, bt
              )));
            }
          }
        }
        // SHOULD ≥2 入边仅为收口语义提示，不阻断 definition 演进中间态（设计 §4.4），故不校验。
      }
      NodeKind::ForEach => {
        // G2 ForEach 动态扇出节点校验（§6.3.2 需改现状③）。
        let (items_path, branch_template, join_target, _max_fanout) = node.for_each().unwrap();
        // items_path 非空 + CEL 可编译（eager 捕获语法错误）。
        if items_path.is_empty() {
          return Err(FlowError::Validation(format!("for_each node '{}' items_path must not be empty", node.id)));
        }
        guard_eval::validate_expression(items_path)
          .map_err(|e| FlowError::Validation(format!("for_each node '{}' items_path invalid: {}", node.id, e)))?;
        // branch_template 引用存在节点 + kind ∈ {Approval, Assignment, EventWait}
        // （MUST NOT Split/ForEach/Merge/Condition/Start/End —— 嵌套扇出本期不开放）。
        let Some(template_node) = nodes.iter().find(|n| n.id == branch_template) else {
          return Err(FlowError::Validation(format!(
            "for_each node '{}' branch_template '{}' references unknown node",
            node.id, branch_template
          )));
        };
        match template_node.kind() {
          NodeKind::Approval | NodeKind::Assignment | NodeKind::EventWait => {}
          other => {
            return Err(FlowError::Validation(format!(
              "for_each node '{}' branch_template '{}' must be APPROVAL/ASSIGNMENT/EVENT_WAIT kind (no nested fan-out); got {:?}",
              node.id, branch_template, other
            )));
          }
        }
        if !template_node.context_writes().is_empty() {
          return Err(FlowError::Validation(format!(
            "for_each node '{}' branch_template '{}' must not declare context_writes",
            node.id, branch_template
          )));
        }
        // join_target 引用存在节点 + kind = ParallelJoin。
        let Some(join_node) = nodes.iter().find(|n| n.id == join_target) else {
          return Err(FlowError::Validation(format!(
            "for_each node '{}' join_target '{}' references unknown node",
            node.id, join_target
          )));
        };
        if join_node.kind() != NodeKind::ParallelJoin {
          return Err(FlowError::Validation(format!(
            "for_each node '{}' join_target '{}' must be a ParallelJoin (got {:?})",
            node.id,
            join_target,
            join_node.kind()
          )));
        }
        // branch_template 须有一条出边汇入 join_target（signal 随其 kind：
        // Approval→APPROVED / Assignment→COMPLETED / EventWait→EVENT_RECEIVED）。
        let has_join_edge = transitions
          .iter()
          .any(|t| t.from == branch_template && t.to == join_target && is_join_incoming_signal(t.on_signal));
        if !has_join_edge {
          return Err(FlowError::Validation(format!(
            "for_each node '{}' branch_template '{}' must have an outgoing transition (APPROVED/COMPLETED/EVENT_RECEIVED) into join_target '{}'",
            node.id, branch_template, join_target
          )));
        }
      }
      NodeKind::SubWorkflow => {
        // G3 SubWorkflow（P4.2）节点校验（gaps §6.3.3）。
        let (child_flow_type, _context_path, on_child_failed_target) = node.sub_workflow().unwrap();
        if child_flow_type.is_empty() {
          return Err(FlowError::Validation(format!(
            "sub_workflow node '{}' child_flow_type must not be empty",
            node.id
          )));
        }
        // 必须有一条 EVENT_RECEIVED 出边：子终态经回灌父 EVENT_RECEIVED 推进下游；缺失则父挂起后永久卡死。
        let has_signal_transition =
          transitions.iter().any(|t| t.from == node.id && t.on_signal == SignalType::EventReceived);
        if !has_signal_transition {
          return Err(FlowError::Validation(format!(
            "sub_workflow node '{}' must have an EVENT_RECEIVED outgoing transition (to advance after child completes)",
            node.id
          )));
        }
        // on_child_failed_target 若设则必须引用存在节点
        if let Some(target) = on_child_failed_target
          && !seen_ids.contains(target)
        {
          return Err(FlowError::Validation(format!(
            "sub_workflow node '{}' on_child_failed_target '{}' references unknown node",
            node.id, target
          )));
        }
      }
      _ => {}
    }
  }

  // transitions.from/to 引用存在节点
  for t in transitions {
    if !seen_ids.contains(t.from.as_str()) {
      return Err(FlowError::Validation(format!("transition.from '{}' references unknown node", t.from)));
    }
    if !seen_ids.contains(t.to.as_str()) {
      return Err(FlowError::Validation(format!("transition.to '{}' references unknown node", t.to)));
    }
  }

  Ok(())
}

/// 缺口 C（topology 只读，§5.3.5）：静态拓扑 lint 的单条发现。
///
/// 展示层契约 —— **纯派生视图**，不进实例状态机、不持久化、不内嵌审计字段。
/// `severity` ∈ {"error","warning"}；`code` 是稳定机读码（前端映射 i18n key）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
  pub severity: String,
  pub code: String,
  pub node_id: Option<String>,
  pub message: String,
}

impl LintFinding {
  fn error(code: &str, node_id: Option<&str>, message: String) -> Self {
    Self { severity: "error".into(), code: code.into(), node_id: node_id.map(str::to_string), message }
  }
  fn warning(code: &str, node_id: Option<&str>, message: String) -> Self {
    Self { severity: "warning".into(), code: code.into(), node_id: node_id.map(str::to_string), message }
  }

  /// D1 dry-run（hetuflow-topology-edit-ui.md）：把 `validate_definition` 的单条字符串错误
  /// 包成一条 `severity="error"` / `code="validation_error"` 的 finding，供编辑期前端复用
  /// `WorkflowGraphLintFinding` 渲染。
  ///
  /// `node_id` 为 **best-effort**：validate 错误文案把节点 id 嵌在首个单引号对里
  /// （如 `approval node 'approval_1' assignee_role ...` / `transition.from 'x' references ...`），
  /// 取首个单引号包裹的 token；解析不出（无引号）则留空 —— 不承诺总能定位到节点。
  pub fn validation_error(message: &str) -> Self {
    let node_id = extract_first_quoted(message);
    Self { severity: "error".into(), code: "validation_error".into(), node_id, message: message.to_string() }
  }
}

/// 取字符串里首个 `'...'` 单引号包裹的 token（best-effort node_id 抽取）。无引号对 → None。
fn extract_first_quoted(s: &str) -> Option<String> {
  let start = s.find('\'')?;
  let rest = &s[start + 1..];
  let end = rest.find('\'')?;
  let token = &rest[..end];
  if token.is_empty() { None } else { Some(token.to_string()) }
}

/// 收集某节点的全部静态 outgoing target（沿 transitions.to + 各 routing config 的内嵌目标）。
///
/// 复用现有 NodeConfig accessor（`condition()` / `parallel_split()` / `event_wait()`），
/// 不重复解构。这是 lint 可达性遍历与死锁检测的公共「出边集合」。
fn static_out_targets(node: &WorkflowNode, transitions: &[WorkflowTransition]) -> Vec<String> {
  let mut out: Vec<String> = transitions.iter().filter(|t| t.from == node.id).map(|t| t.to.clone()).collect();
  match node.kind() {
    NodeKind::ParallelSplit => {
      if let Some(branches) = node.parallel_split() {
        out.extend(branches.iter().cloned());
      }
    }
    NodeKind::Condition => {
      if let Some((default_target, branches)) = node.condition() {
        out.push(default_target.to_string());
        out.extend(branches.iter().map(|b| b.target.clone()));
      }
    }
    NodeKind::EventWait => {
      if let Some((_, _, Some(timeout_target))) = node.event_wait() {
        out.push(timeout_target.to_string());
      }
    }
    _ => {}
  }
  out
}

/// 缺口 C（§5.3.5）：静态拓扑 lint —— 全图算法，输入 = `nodes + transitions`，纯函数无副作用。
///
/// 补全 `validate_definition`（节点级发布 gate）未覆盖的**全图维度**，对引擎零侵入
/// （OD-2：本期不下沉 validate）。4 检测项：
/// - `dangling_edge`(error)：transition.from/to 引用不存在 node id。
/// - `unreachable_node`(error)：从唯一 Start 出发沿 static_out_targets 可达性遍历未触达的非 Start 节点。
/// - `deadlock_node`(error)：非 End 节点且无任何 outgoing 推进路径（EventWait 既无 EVENT_RECEIVED
///   出边又无 timeout_target；Approval 无 APPROVED 出边等）。
/// - `parallel_unclosed`(warning)：ParallelSplit 的某 branch_target 不可达任何 ParallelJoin。
pub fn lint_definition(nodes: &[WorkflowNode], transitions: &[WorkflowTransition]) -> Vec<LintFinding> {
  let mut findings = Vec::new();
  let node_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();

  // ---- (1) dangling_edge：transition 端点引用不存在节点 ----
  for t in transitions {
    if !node_ids.contains(t.from.as_str()) {
      findings.push(LintFinding::error(
        "dangling_edge",
        Some(&t.from),
        format!("transition.from '{}' references a non-existent node", t.from),
      ));
    }
    if !node_ids.contains(t.to.as_str()) {
      findings.push(LintFinding::error(
        "dangling_edge",
        Some(&t.to),
        format!("transition.to '{}' references a non-existent node", t.to),
      ));
    }
  }

  // ---- (2) unreachable_node：从唯一 Start BFS（沿 static_out_targets，仅跟随存在的节点）----
  let start_nodes: Vec<&WorkflowNode> = nodes.iter().filter(|n| n.kind() == NodeKind::Start).collect();
  // 仅在恰好一个 Start 时做可达性遍历（0 / 多 Start 是 validate_definition 的职责，lint 不重复报）。
  if start_nodes.len() == 1 {
    let mut reachable: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = vec![start_nodes[0].id.as_str()];
    while let Some(cur) = stack.pop() {
      if !reachable.insert(cur) {
        continue;
      }
      let Some(node) = nodes.iter().find(|n| n.id == cur) else { continue };
      for target in static_out_targets(node, transitions) {
        // 只跟随存在的目标（悬空目标已由 (1) 报告）。
        if node_ids.contains(target.as_str()) && !reachable.contains(target.as_str()) {
          stack.push(nodes.iter().find(|n| n.id == target).map(|n| n.id.as_str()).unwrap());
        }
      }
    }
    for node in nodes {
      if node.kind() != NodeKind::Start && !reachable.contains(node.id.as_str()) {
        findings.push(LintFinding::error(
          "unreachable_node",
          Some(&node.id),
          format!("node '{}' is not reachable from the start node", node.id),
        ));
      }
    }
  }

  // ---- (3) deadlock_node：非 End 节点无任何 outgoing 推进路径 ----
  for node in nodes {
    match node.kind() {
      // End 是合法终态；Start 在 v1 仅作可达性起点（其 RESUBMITTED 出边由 decide_start 旁路，
      // 不要求 outgoing 推进边），故不计入死锁。
      NodeKind::End | NodeKind::Start => continue,
      _ => {}
    }
    if static_out_targets(node, transitions).is_empty() {
      let detail = match node.kind() {
        NodeKind::EventWait => " (no EVENT_RECEIVED outgoing transition and no timeout_target)",
        NodeKind::Approval => " (no APPROVED outgoing transition)",
        _ => "",
      };
      findings.push(LintFinding::error(
        "deadlock_node",
        Some(&node.id),
        format!("node '{}' has no outgoing advance path and is not an end node{detail}", node.id),
      ));
    }
  }

  // ---- (4) parallel_unclosed（warning）：ParallelSplit 某 branch 不可达任何 ParallelJoin ----
  for node in nodes {
    if node.kind() != NodeKind::ParallelSplit {
      continue;
    }
    let Some(branch_targets) = node.parallel_split() else { continue };
    for branch in branch_targets {
      // 跳过悬空 branch（已由 (1)/(2) 报告）。
      if !node_ids.contains(branch.as_str()) {
        continue;
      }
      if !branch_reaches_join(branch, nodes, transitions) {
        findings.push(LintFinding::warning(
          "parallel_unclosed",
          Some(branch),
          format!("parallel branch '{}' (from split '{}') does not reach any ParallelJoin", branch, node.id),
        ));
      }
    }
  }

  findings
}

/// (4) 辅助：从某分支节点沿 static_out_targets 遍历，能否到达任一 ParallelJoin。
fn branch_reaches_join(start: &str, nodes: &[WorkflowNode], transitions: &[WorkflowTransition]) -> bool {
  let mut visited: HashSet<&str> = HashSet::new();
  let mut stack: Vec<&str> = vec![start];
  while let Some(cur) = stack.pop() {
    if !visited.insert(cur) {
      continue;
    }
    let Some(node) = nodes.iter().find(|n| n.id == cur) else { continue };
    if node.kind() == NodeKind::ParallelJoin {
      return true;
    }
    for target in static_out_targets(node, transitions) {
      if let Some(next) = nodes.iter().find(|n| n.id == target)
        && !visited.contains(next.id.as_str())
      {
        stack.push(next.id.as_str());
      }
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use hetuflow_core::{
    AssignmentConfig, ConditionBranch, NodeConfig, NotificationChannelPolicy, NotificationConfig, RecipientSelector,
  };

  fn start_node() -> WorkflowNode {
    WorkflowNode { id: "start".into(), name: "Start".into(), context_writes: Vec::new(), config: NodeConfig::Start }
  }
  fn end_node() -> WorkflowNode {
    WorkflowNode {
      id: "end".into(),
      name: "End".into(),
      context_writes: Vec::new(),
      config: NodeConfig::End { result_code: None },
    }
  }
  fn end_with(id: &str, result_code: Option<&str>) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "End".into(),
      context_writes: Vec::new(),
      config: NodeConfig::End { result_code: result_code.map(str::to_string) },
    }
  }
  fn approval(id: &str, role: &str) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "审批".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some(role.into()),
        sla_seconds: None,
        escalation_seconds: None,
        escalation_target_role: None,
      },
    }
  }
  fn condition(id: &str, default_target: &str, branches: Vec<(&str, &str)>) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "Condition".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Condition {
        default_target: default_target.into(),
        branches: branches
          .into_iter()
          .map(|(g, t)| ConditionBranch { guard_expression: g.into(), target: t.into() })
          .collect(),
      },
    }
  }
  fn notification(id: &str, selector: Option<RecipientSelector>) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "Notification".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Notification(NotificationConfig {
        template_code: Some("workflow_notice".into()),
        recipient_selector: selector,
        channel_policy: NotificationChannelPolicy { notification_types: vec!["NOTIFICATION_TYPE_IN_APP".into()] },
        template_args: Default::default(),
        reference_type: Some("workflow".into()),
        purpose: Some("workflow_notification".into()),
        visibility: Some("VISIBILITY_ACTION".into()),
      }),
    }
  }
  fn assignment(id: &str, selector: Option<RecipientSelector>, queue_code: Option<&str>) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "Assignment".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Assignment(AssignmentConfig {
        assignee_selector: selector,
        queue_code: queue_code.map(str::to_string),
        sla_seconds: Some(3600),
      }),
    }
  }
  fn tr(from: &str, to: &str, sig: SignalType) -> WorkflowTransition {
    WorkflowTransition { from: from.into(), to: to.into(), on_signal: sig }
  }

  /// 测试 helper：取 decide_advance 的 outcome（丢弃 trace）。
  fn da(
    nodes: &[WorkflowNode],
    transitions: &[WorkflowTransition],
    current_node: &str,
    signal: SignalType,
    context: Option<&serde_json::Value>,
  ) -> AdvanceOutcome {
    decide_advance(nodes, transitions, current_node, signal, context).0
  }

  /// 测试 helper：取 decide_advance 的 (outcome, trace)。
  fn da_trace(
    nodes: &[WorkflowNode],
    transitions: &[WorkflowTransition],
    current_node: &str,
    signal: SignalType,
    context: Option<&serde_json::Value>,
  ) -> (AdvanceOutcome, ResolveTrace) {
    decide_advance(nodes, transitions, current_node, signal, context)
  }

  /// 测试 helper：取 decide_event_wait_timeout 的 outcome（丢弃 trace）。
  fn dewt(
    nodes: &[WorkflowNode],
    transitions: &[WorkflowTransition],
    current_node: &str,
    context: Option<&serde_json::Value>,
  ) -> AdvanceOutcome {
    decide_event_wait_timeout(nodes, transitions, current_node, context).0
  }

  fn linear_single() -> (Vec<WorkflowNode>, Vec<WorkflowTransition>) {
    (
      vec![start_node(), approval("approval_1", "facility_admin"), end_node()],
      vec![
        tr("start", "approval_1", SignalType::Resubmitted),
        tr("approval_1", "end", SignalType::Approved),
        tr("approval_1", "end", SignalType::Rejected),
      ],
    )
  }

  #[test]
  fn validate_passes_linear() {
    let (n, t) = linear_single();
    assert!(validate_definition(&n, &t).is_ok());
  }

  #[test]
  fn validate_rejects_parallel_timer_not_condition() {
    let (mut n, t) = linear_single();
    n.push(WorkflowNode { id: "p".into(), name: "p".into(), context_writes: Vec::new(), config: NodeConfig::Parallel });
    assert!(validate_definition(&n, &t).unwrap_err().to_string().contains("Parallel"));
  }

  #[test]
  fn validate_accepts_condition_with_default() {
    // approval → condition → (amount > 100 → end_high) / (default → end_low)
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      condition("cond_1", "end_low", vec![("context.amount > 100", "end_high")]),
      end_with("end_high", Some("approved")),
      end_with("end_low", Some("rejected")),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "cond_1", SignalType::Approved),
      tr("approval_1", "end_low", SignalType::Rejected),
    ];
    assert!(validate_definition(&nodes, &transitions).is_ok());
  }

  #[test]
  fn validate_rejects_condition_missing_default() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      condition("cond_1", "", vec![("context.amount > 100", "end")]),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "cond_1", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("default_target"), "expected default_target error, got: {err}");
  }

  #[test]
  fn validate_rejects_condition_with_bad_target() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      condition("cond_1", "end", vec![("context.amount > 100", "missing_node")]),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "cond_1", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("missing_node"), "expected unknown-node error, got: {err}");
  }

  #[test]
  fn validate_rejects_condition_with_bad_guard() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      condition("cond_1", "end", vec![("context.amount >>", "end")]),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "cond_1", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("guard"), "expected guard parse error, got: {err}");
  }

  #[test]
  fn validate_accepts_multi_end() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      end_with("end_a", Some("approved")),
      end_with("end_r", Some("rejected")),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "end_a", SignalType::Approved),
      tr("approval_1", "end_r", SignalType::Rejected),
    ];
    assert!(validate_definition(&nodes, &transitions).is_ok());
  }

  #[test]
  fn decide_start_picks_first_approval() {
    let (n, _) = linear_single();
    let (node, role) = decide_start(&n).unwrap();
    assert_eq!(node, "approval_1");
    assert_eq!(role.as_deref(), Some("facility_admin"));
  }

  #[test]
  fn decide_advance_linear_completes_at_end() {
    let (n, t) = linear_single();
    assert_eq!(
      da(&n, &t, "approval_1", SignalType::Approved, None),
      AdvanceOutcome::Complete { result: WorkflowResult::Approved }
    );
    assert_eq!(
      da(&n, &t, "approval_1", SignalType::Rejected, None),
      AdvanceOutcome::Complete { result: WorkflowResult::Rejected }
    );
  }

  #[test]
  fn decide_advance_condition_branch_hit() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      condition("cond_1", "end_low", vec![("context.amount > 100", "end_high")]),
      end_with("end_high", Some("approved")),
      end_with("end_low", Some("rejected")),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "cond_1", SignalType::Approved)];
    // amount = 200 → hits branch → end_high (approved)
    let ctx = serde_json::json!({"amount": 200});
    assert_eq!(
      da(&nodes, &transitions, "approval_1", SignalType::Approved, Some(&ctx)),
      AdvanceOutcome::Complete { result: WorkflowResult::Approved }
    );
    // amount = 50 → no branch → default → end_low (rejected)
    let ctx = serde_json::json!({"amount": 50});
    assert_eq!(
      da(&nodes, &transitions, "approval_1", SignalType::Approved, Some(&ctx)),
      AdvanceOutcome::Complete { result: WorkflowResult::Rejected }
    );
  }

  fn event_wait(id: &str, event_type: &str, timeout_target: Option<&str>) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "EventWait".into(),
      context_writes: Vec::new(),
      config: NodeConfig::EventWait {
        event_type: event_type.into(),
        timeout_seconds: timeout_target.map(|_| 3600),
        timeout_target: timeout_target.map(str::to_string),
        correlation_key: Some("corr-1".into()),
        source: None,
      },
    }
  }

  #[test]
  fn validate_accepts_event_wait_with_signal_transition() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "momo.work_order.completed", None),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "wait_1", SignalType::Approved),
      tr("wait_1", "end", SignalType::EventReceived),
    ];
    assert!(validate_definition(&nodes, &transitions).is_ok());
  }

  #[test]
  fn validate_accepts_event_wait_with_timeout_target_only() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", Some("end_timeout")),
      end_with("end_timeout", Some("rejected")),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "wait_1", SignalType::Approved)];
    assert!(validate_definition(&nodes, &transitions).is_ok());
  }

  #[test]
  fn validate_rejects_event_wait_with_empty_event_type() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "", Some("end")),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "wait_1", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("event_type"), "got: {err}");
  }

  #[test]
  fn validate_rejects_event_wait_with_no_outgoing_path() {
    // 无 EVENT_RECEIVED transition 且无 timeout_target → 永久卡死
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", None),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "wait_1", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("EVENT_RECEIVED") || err.contains("timeout_target"), "got: {err}");
  }

  #[test]
  fn validate_rejects_event_wait_with_unknown_timeout_target() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", Some("nope")),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "wait_1", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("unknown node"), "got: {err}");
  }

  #[test]
  fn decide_advance_event_received_completes_to_end() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", Some("end")),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "wait_1", SignalType::Approved),
      tr("wait_1", "end", SignalType::EventReceived),
    ];
    assert_eq!(
      da(&nodes, &transitions, "wait_1", SignalType::EventReceived, None),
      AdvanceOutcome::Complete { result: WorkflowResult::Approved }
    );
  }

  #[test]
  fn decide_event_wait_timeout_advances_to_target() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", Some("end_to")),
      end_with("end_to", Some("rejected")),
    ];
    // 无 EVENT_RECEIVED transition 也合法（仅靠 timeout_target 推进）
    assert_eq!(dewt(&nodes, &[], "wait_1", None), AdvanceOutcome::Complete { result: WorkflowResult::Rejected });
  }

  #[test]
  fn decide_event_wait_timeout_errors_when_no_target() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", None),
      end_node(),
    ];
    assert_eq!(dewt(&nodes, &[], "wait_1", None), AdvanceOutcome::Error);
  }

  #[test]
  fn validate_accepts_notification_with_completed_transition() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      notification("notify_1", Some(RecipientSelector::Role { role_code: "facility_admin".into(), facility_id: None })),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "notify_1", SignalType::Approved),
      tr("notify_1", "end", SignalType::Completed),
    ];
    validate_definition(&nodes, &transitions).unwrap();
  }

  #[test]
  fn validate_rejects_notification_without_recipient_selector() {
    let nodes =
      vec![start_node(), approval("approval_1", "facility_admin"), notification("notify_1", None), end_node()];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "notify_1", SignalType::Approved),
      tr("notify_1", "end", SignalType::Completed),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("recipient_selector"), "got: {err}");
  }

  #[test]
  fn validate_rejects_notification_without_completed_transition() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      notification("notify_1", Some(RecipientSelector::UserIds { user_ids: vec!["user-1".into()] })),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "notify_1", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("COMPLETED"), "got: {err}");
  }

  #[test]
  fn decide_advance_notification_completed_can_target_assignment() {
    let nodes = vec![
      start_node(),
      notification("notify_1", Some(RecipientSelector::UserIds { user_ids: vec!["user-1".into()] })),
      assignment("assign_1", None, Some("nurse_queue")),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "notify_1", SignalType::Resubmitted),
      tr("notify_1", "assign_1", SignalType::Completed),
      tr("assign_1", "end", SignalType::Completed),
    ];
    assert_eq!(
      da(&nodes, &transitions, "notify_1", SignalType::Completed, None),
      AdvanceOutcome::Advance { target_node: "assign_1".into(), assignee_role: None }
    );
  }

  #[test]
  fn validate_accepts_assignment_with_queue_or_selector() {
    let selector = RecipientSelector::Role { role_code: "nurse".into(), facility_id: None };
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      assignment("assign_1", Some(selector), None),
      assignment("assign_2", None, Some("nurse_queue")),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "assign_1", SignalType::Approved),
      tr("assign_1", "assign_2", SignalType::Completed),
      tr("assign_2", "end", SignalType::Completed),
    ];
    validate_definition(&nodes, &transitions).unwrap();
  }

  #[test]
  fn validate_rejects_assignment_without_assignee_or_queue() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      assignment("assign_1", None, None),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "assign_1", SignalType::Approved),
      tr("assign_1", "end", SignalType::Completed),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("assignee_selector") || err.contains("queue_code"), "got: {err}");
  }

  #[test]
  fn decide_advance_assignment_completed_reaches_end() {
    let nodes = vec![start_node(), assignment("assign_1", None, Some("nurse_queue")), end_node()];
    let transitions =
      vec![tr("start", "assign_1", SignalType::Resubmitted), tr("assign_1", "end", SignalType::Completed)];
    assert_eq!(
      da(&nodes, &transitions, "assign_1", SignalType::Completed, None),
      AdvanceOutcome::Complete { result: WorkflowResult::Approved }
    );
  }

  #[test]
  fn resolve_to_event_wait_returns_advance_no_role() {
    // approval_1 → wait_1（resolve_target 命中 EventWait → Advance{assignee_role=None}）
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", Some("end")),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "wait_1", SignalType::Approved),
      tr("wait_1", "end", SignalType::EventReceived),
    ];
    assert_eq!(
      da(&nodes, &transitions, "approval_1", SignalType::Approved, None),
      AdvanceOutcome::Advance { target_node: "wait_1".into(), assignee_role: None }
    );
  }

  fn parallel_split(id: &str, branches: Vec<&str>) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "Split".into(),
      context_writes: Vec::new(),
      config: NodeConfig::ParallelSplit { branch_targets: branches.into_iter().map(String::from).collect() },
    }
  }
  fn parallel_join(id: &str) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "Join".into(),
      context_writes: Vec::new(),
      config: NodeConfig::ParallelJoin { strategy: hetuflow_core::JoinStrategy::WaitAll },
    }
  }
  fn parallel_join_with(id: &str, strategy: hetuflow_core::JoinStrategy) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "Join".into(),
      context_writes: Vec::new(),
      config: NodeConfig::ParallelJoin { strategy },
    }
  }

  fn parallel_def() -> (Vec<WorkflowNode>, Vec<WorkflowTransition>) {
    // start → approval_0 → split → [approval_a, approval_b] → join → end
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("approval_b", "join_1", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
    ];
    (nodes, transitions)
  }

  #[test]
  fn validate_accepts_basic_parallel_split_join() {
    let (n, t) = parallel_def();
    validate_definition(&n, &t).unwrap();
  }

  // ---- P4.1 Mixed Parallel（静态异质并行）----

  fn mixed_parallel_def() -> (Vec<WorkflowNode>, Vec<WorkflowTransition>) {
    // start → approval_0 → split → [approval_a, assign_b, event_c, notify_d] → join(WaitAll) → end
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "assign_b", "event_c", "notify_d"]),
      approval("approval_a", "facility_admin"),
      assignment("assign_b", None, Some("queue_b")),
      event_wait("event_c", "evt.c", None),
      notification("notify_d", Some(RecipientSelector::Role { role_code: "nurse".into(), facility_id: None })),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("assign_b", "join_1", SignalType::Completed),
      tr("event_c", "join_1", SignalType::EventReceived),
      tr("notify_d", "join_1", SignalType::Completed),
      tr("join_1", "end", SignalType::Approved),
    ];
    (nodes, transitions)
  }

  #[test]
  fn validate_accepts_mixed_parallel_branches() {
    let (n, t) = mixed_parallel_def();
    validate_definition(&n, &t).unwrap();
  }

  #[test]
  fn decide_advance_mixed_split_derives_branch_activity_types() {
    let (n, t) = mixed_parallel_def();
    match da(&n, &t, "approval_0", SignalType::Approved, None) {
      AdvanceOutcome::AdvanceMulti { targets } => {
        assert_eq!(targets.len(), 4);
        let by_id = |id: &str| targets.iter().find(|x| x.node_id == id).unwrap();
        assert_eq!(by_id("approval_a").activity_type, ActivityType::Approval);
        assert_eq!(by_id("assign_b").activity_type, ActivityType::Assignment);
        assert_eq!(by_id("event_c").activity_type, ActivityType::EventWait);
        assert_eq!(by_id("notify_d").activity_type, ActivityType::Notification);
      }
      other => panic!("expected AdvanceMulti, got {other:?}"),
    }
  }

  #[test]
  fn validate_rejects_mixed_split_with_routing_branch_kind() {
    // branch 指向 Condition（路由节点）→ 拒绝（嵌套 / 路由 kind 不允许）
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "cond_b"]),
      approval("approval_a", "facility_admin"),
      condition("cond_b", "end", vec![]),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("cond_b", "join_1", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(
      err.contains("ASSIGNMENT") || err.contains("NOTIFICATION") || err.contains("EVENT_WAIT") || err.contains("kind"),
      "expected branch-kind rejection, got: {err}"
    );
  }

  #[test]
  fn validate_rejects_split_with_fewer_than_2_branches() {
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a"]),
      approval("approval_a", "facility_admin"),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains(">= 2 branch_targets"), "got: {err}");
  }

  #[test]
  fn validate_rejects_branches_diverging_to_different_joins() {
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      parallel_join("join_1"),
      parallel_join("join_2"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("approval_b", "join_2", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
      tr("join_2", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("SAME ParallelJoin"), "got: {err}");
  }

  #[test]
  fn validate_rejects_branch_not_targeting_join() {
    // branch APPROVED 指向 end_low（非 ParallelJoin）
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      parallel_join("join_1"),
      end_with("end_low", Some("rejected")),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "end_low", SignalType::Approved), // 应指 join，验证应拒绝
      tr("approval_b", "join_1", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("must target a ParallelJoin"), "got: {err}");
  }

  #[test]
  fn validate_rejects_join_without_outgoing_transition() {
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("approval_b", "join_1", SignalType::Approved),
      // 缺 join_1 → end 出边
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("outgoing transition"), "got: {err}");
  }

  #[test]
  fn decide_advance_split_returns_multi_targets() {
    let (n, t) = parallel_def();
    let outcome = da(&n, &t, "approval_0", SignalType::Approved, None);
    match outcome {
      AdvanceOutcome::AdvanceMulti { targets } => {
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].node_id, "approval_a");
        assert_eq!(targets[0].activity_type, hetuflow_core::ActivityType::Approval);
        assert_eq!(targets[0].assignee_role.as_deref(), Some("facility_admin"));
        assert_eq!(targets[1].node_id, "approval_b");
        assert_eq!(targets[1].assignee_role.as_deref(), Some("facility_director"));
      }
      other => panic!("expected AdvanceMulti, got {other:?}"),
    }
  }

  #[test]
  fn decide_advance_branch_approved_targets_join() {
    let (n, t) = parallel_def();
    // 单条分支 approval_a Approved → transition.to = join_1；resolve_target 返 Advance{target=join_1}
    let outcome = da(&n, &t, "approval_a", SignalType::Approved, None);
    match outcome {
      AdvanceOutcome::Advance { target_node, assignee_role } => {
        assert_eq!(target_node, "join_1");
        assert_eq!(assignee_role, None); // Join 无 assignee_role
      }
      other => panic!("expected Advance to join_1, got {other:?}"),
    }
  }

  // ---- G1.5：JoinStrategy validate ----

  fn parallel_def_with(strategy: hetuflow_core::JoinStrategy) -> (Vec<WorkflowNode>, Vec<WorkflowTransition>) {
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b", "approval_c"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      approval("approval_c", "facility_admin"),
      parallel_join_with("join_1", strategy),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("approval_b", "join_1", SignalType::Approved),
      tr("approval_c", "join_1", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
    ];
    (nodes, transitions)
  }

  #[test]
  fn validate_accepts_wait_any_strategy() {
    let (n, t) = parallel_def_with(hetuflow_core::JoinStrategy::WaitAny);
    validate_definition(&n, &t).unwrap();
  }

  #[test]
  fn validate_accepts_n_of_m_within_bounds() {
    let (n, t) = parallel_def_with(hetuflow_core::JoinStrategy::NOfM(2));
    validate_definition(&n, &t).unwrap();
  }

  #[test]
  fn validate_rejects_n_of_m_zero() {
    let (n, t) = parallel_def_with(hetuflow_core::JoinStrategy::NOfM(0));
    let err = validate_definition(&n, &t).unwrap_err().to_string();
    assert!(err.contains("min_completions >= 1"), "got: {err}");
  }

  #[test]
  fn validate_rejects_n_of_m_exceeds_incoming() {
    let (n, t) = parallel_def_with(hetuflow_core::JoinStrategy::NOfM(5));
    let err = validate_definition(&n, &t).unwrap_err().to_string();
    assert!(err.contains("exceeds incoming branches"), "got: {err}");
  }

  // ---- F2：escalation validate ----

  fn approval_with_escalation(
    id: &str,
    role: &str,
    sla: Option<i32>,
    esc_secs: Option<i32>,
    esc_role: Option<&str>,
  ) -> WorkflowNode {
    use hetuflow_core::NodeConfig;
    WorkflowNode {
      id: id.into(),
      name: "审批".into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some(role.into()),
        sla_seconds: sla,
        escalation_seconds: esc_secs,
        escalation_target_role: esc_role.map(String::from),
      },
    }
  }

  #[test]
  fn validate_accepts_escalation_pair() {
    let nodes = vec![
      start_node(),
      approval_with_escalation("approval_1", "facility_admin", Some(30), Some(120), Some("facility_director")),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "end", SignalType::Approved)];
    validate_definition(&nodes, &transitions).unwrap();
  }

  #[test]
  fn validate_rejects_escalation_less_than_or_equal_sla() {
    let nodes = vec![
      start_node(),
      approval_with_escalation("approval_1", "facility_admin", Some(120), Some(60), Some("facility_director")),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "end", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("must be > sla_seconds"), "got: {err}");
  }

  #[test]
  fn validate_rejects_escalation_role_missing() {
    let nodes = vec![
      start_node(),
      approval_with_escalation("approval_1", "facility_admin", Some(30), Some(120), None),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "end", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("escalation_target_role missing"), "got: {err}");
  }

  #[test]
  fn validate_rejects_escalation_seconds_missing() {
    let nodes = vec![
      start_node(),
      approval_with_escalation("approval_1", "facility_admin", Some(30), None, Some("facility_director")),
      end_node(),
    ];
    let transitions =
      vec![tr("start", "approval_1", SignalType::Resubmitted), tr("approval_1", "end", SignalType::Approved)];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("escalation_seconds missing"), "got: {err}");
  }

  #[test]
  fn decide_advance_condition_to_approval() {
    // approval_1 → cond → (amount > 100 → approval_2) / (default → end)
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      condition("cond_1", "end", vec![("context.amount > 100", "approval_2")]),
      approval("approval_2", "facility_director"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "cond_1", SignalType::Approved),
      tr("approval_2", "end", SignalType::Approved),
    ];
    let ctx = serde_json::json!({"amount": 500});
    // 命中 branch → 解析到 approval_2 → Advance
    assert_eq!(
      da(&nodes, &transitions, "approval_1", SignalType::Approved, Some(&ctx)),
      AdvanceOutcome::Advance { target_node: "approval_2".into(), assignee_role: Some("facility_director".into()) }
    );
  }

  // ============================================================
  // 缺口 B：串行 Merge（no-op 路由收口节点）—— BDD §4.5
  // ============================================================

  fn merge(id: &str) -> WorkflowNode {
    WorkflowNode { id: id.into(), name: "Merge".into(), context_writes: Vec::new(), config: NodeConfig::Merge }
  }

  /// §4.5 scenario 1：条件分支经 Merge 穿透到公共后继（resolve_target 落 Approval）。
  /// approval_a / approval_b 两路 APPROVED → merge_x → approval_final。
  #[test]
  fn decide_advance_merge_passthrough_to_approval_with_trace() {
    let nodes = vec![
      start_node(),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      merge("merge_x"),
      approval("approval_final", "facility_manager"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_x", SignalType::Approved),
      tr("approval_b", "merge_x", SignalType::Approved),
      tr("merge_x", "approval_final", SignalType::Approved),
      tr("approval_final", "end", SignalType::Approved),
    ];
    let (outcome, trace) = da_trace(&nodes, &transitions, "approval_a", SignalType::Approved, None);
    assert_eq!(
      outcome,
      AdvanceOutcome::Advance { target_node: "approval_final".into(), assignee_role: Some("facility_manager".into()) }
    );
    // trace 记录穿过的 Merge，service 据此 append MERGE_PASSED
    assert_eq!(trace.merges_passed, vec!["merge_x".to_string()]);
  }

  /// §4.5 scenario 2：Merge 穿透后落 End → Complete，result 取自 End（Merge 不注入 result）。
  #[test]
  fn decide_advance_merge_passthrough_to_end_takes_end_result() {
    let nodes = vec![
      start_node(),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      merge("merge_done"),
      end_with("end", Some("approved")),
    ];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_done", SignalType::Approved),
      tr("approval_b", "merge_done", SignalType::Approved),
      tr("merge_done", "end", SignalType::Approved),
    ];
    let (outcome, trace) = da_trace(&nodes, &transitions, "approval_b", SignalType::Approved, None);
    assert_eq!(outcome, AdvanceOutcome::Complete { result: WorkflowResult::Approved });
    assert_eq!(trace.merges_passed, vec!["merge_done".to_string()]);
  }

  /// §4.5 scenario 3：多级 Merge 链穿透（漏斗套漏斗），trace 记录两个 Merge。
  #[test]
  fn decide_advance_multi_merge_chain_passthrough() {
    let nodes = vec![
      start_node(),
      approval("approval_a", "facility_admin"),
      merge("merge_1"),
      merge("merge_2"),
      approval("approval_final", "facility_manager"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_1", SignalType::Approved),
      tr("merge_1", "merge_2", SignalType::Approved),
      tr("merge_2", "approval_final", SignalType::Approved),
      tr("approval_final", "end", SignalType::Approved),
    ];
    let (outcome, trace) = da_trace(&nodes, &transitions, "approval_a", SignalType::Approved, None);
    assert!(matches!(outcome, AdvanceOutcome::Advance { ref target_node, .. } if target_node == "approval_final"));
    assert_eq!(trace.merges_passed, vec!["merge_1".to_string(), "merge_2".to_string()]);
  }

  /// §4.5 scenario：运行时 Merge 环路 → resolve_target cycle 检测 → Error（不死循环）。
  #[test]
  fn decide_advance_merge_cycle_errors() {
    let nodes = vec![
      start_node(),
      approval("approval_a", "facility_admin"),
      merge("merge_a"),
      merge("merge_b"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_a", SignalType::Approved),
      tr("merge_a", "merge_b", SignalType::Approved),
      tr("merge_b", "merge_a", SignalType::Approved), // 环
    ];
    let (outcome, _trace) = da_trace(&nodes, &transitions, "approval_a", SignalType::Approved, None);
    assert_eq!(outcome, AdvanceOutcome::Error);
  }

  /// §4.5 scenario 1（validate）：合法 Merge（恰好一条 APPROVED 出边）通过校验。
  #[test]
  fn validate_accepts_merge_with_single_outgoing() {
    let nodes = vec![
      start_node(),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      merge("merge_x"),
      approval("approval_final", "facility_manager"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_x", SignalType::Approved),
      tr("approval_b", "merge_x", SignalType::Approved),
      tr("merge_x", "approval_final", SignalType::Approved),
      tr("approval_final", "end", SignalType::Approved),
      tr("approval_b", "end", SignalType::Rejected),
    ];
    validate_definition(&nodes, &transitions).unwrap();
  }

  /// §4.5 scenario 4：校验拒绝缺出边的 Merge（死胡同）。
  #[test]
  fn validate_rejects_merge_without_outgoing() {
    let nodes = vec![start_node(), approval("approval_a", "facility_admin"), merge("merge_x"), end_node()];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_x", SignalType::Approved),
      // 缺 merge_x → ... 出边
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("exactly one APPROVED outgoing"), "got: {err}");
  }

  /// §4.5 scenario 5：校验拒绝多出边的 Merge（路由歧义）。
  #[test]
  fn validate_rejects_merge_with_multiple_outgoing() {
    let nodes = vec![
      start_node(),
      approval("approval_a", "facility_admin"),
      merge("merge_x"),
      approval("approval_1", "facility_admin"),
      approval("approval_2", "facility_director"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_x", SignalType::Approved),
      tr("merge_x", "approval_1", SignalType::Approved),
      tr("merge_x", "approval_2", SignalType::Approved), // 第二条 APPROVED 出边
      tr("approval_1", "end", SignalType::Approved),
      tr("approval_2", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("exactly one APPROVED outgoing"), "got: {err}");
  }

  /// §4.5 scenario 6：校验拒绝并行分支汇入 Merge（必须用 ParallelJoin）。
  #[test]
  fn validate_rejects_parallel_branch_converging_to_merge() {
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      merge("merge_x"), // 并行分支错误地汇入 Merge
      approval("approval_final", "facility_manager"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "merge_x", SignalType::Approved),
      tr("approval_b", "merge_x", SignalType::Approved),
      tr("merge_x", "approval_final", SignalType::Approved),
      tr("approval_final", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    // split 分支 APPROVED 必须指向 ParallelJoin（先于 Merge arm 命中），或 Merge arm 拒绝并行汇入
    assert!(err.contains("ParallelJoin") || err.contains("convergence of parallel branches"), "got: {err}");
  }

  /// §4.5 scenario 7：校验拒绝 Merge 出边直接指向 ParallelSplit（不可隐式引爆并行）。
  #[test]
  fn validate_rejects_merge_outgoing_to_parallel_split() {
    let nodes = vec![
      start_node(),
      approval("approval_a", "facility_admin"),
      merge("merge_x"),
      parallel_split("split_1", vec!["approval_b", "approval_c"]),
      approval("approval_b", "facility_admin"),
      approval("approval_c", "facility_director"),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_a", SignalType::Resubmitted),
      tr("approval_a", "merge_x", SignalType::Approved),
      tr("merge_x", "split_1", SignalType::Approved), // Merge → Split：拒绝
      tr("approval_b", "join_1", SignalType::Approved),
      tr("approval_c", "join_1", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("must not target a ParallelSplit"), "got: {err}");
  }

  // ============================================================
  // 缺口 A：rework loop —— decide_advance 的 Returned 分流（BDD §3.5）
  // ============================================================

  /// §3.5 scenario 1：节点声明 RETURNED transition → 精确返工，返回 Rework{rework_target}。
  #[test]
  fn decide_advance_returned_with_rework_transition_returns_rework() {
    // fm_triage_approval → fm_lead_approval；fm_lead RETURNED → fm_triage（退回上一审）
    let nodes = vec![
      start_node(),
      approval("fm_triage_approval", "fm_triage"),
      approval("fm_lead_approval", "fm_lead"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "fm_triage_approval", SignalType::Resubmitted),
      tr("fm_triage_approval", "fm_lead_approval", SignalType::Approved),
      tr("fm_lead_approval", "end", SignalType::Approved),
      tr("fm_lead_approval", "fm_triage_approval", SignalType::Returned),
    ];
    let (outcome, _trace) = da_trace(&nodes, &transitions, "fm_lead_approval", SignalType::Returned, None);
    assert_eq!(
      outcome,
      AdvanceOutcome::Rework { target_node: "fm_triage_approval".into(), assignee_role: Some("fm_triage".into()) }
    );
  }

  /// §3.5 scenario 3：节点无 RETURNED transition → 整体重提，返回 Complete{Returned}。
  #[test]
  fn decide_advance_returned_without_rework_transition_completes_returned() {
    let nodes = vec![start_node(), approval("approval_1", "facility_admin"), end_node()];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "end", SignalType::Approved),
      // 无 RETURNED 出边
    ];
    let (outcome, _trace) = da_trace(&nodes, &transitions, "approval_1", SignalType::Returned, None);
    assert_eq!(outcome, AdvanceOutcome::Complete { result: WorkflowResult::Returned });
  }

  /// 精确返工可穿透 Condition 落到 ReworkTarget（resolve_target 链复用，§3.3.3）。
  #[test]
  fn decide_advance_returned_resolves_through_condition_to_rework_target() {
    // fm_lead RETURNED → cond_back →（default → fm_triage）
    let nodes = vec![
      start_node(),
      approval("fm_triage_approval", "fm_triage"),
      approval("fm_lead_approval", "fm_lead"),
      condition("cond_back", "fm_triage_approval", vec![]),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "fm_triage_approval", SignalType::Resubmitted),
      tr("fm_triage_approval", "fm_lead_approval", SignalType::Approved),
      tr("fm_lead_approval", "end", SignalType::Approved),
      tr("fm_lead_approval", "cond_back", SignalType::Returned),
    ];
    let (outcome, _trace) = da_trace(&nodes, &transitions, "fm_lead_approval", SignalType::Returned, None);
    assert_eq!(
      outcome,
      AdvanceOutcome::Rework { target_node: "fm_triage_approval".into(), assignee_role: Some("fm_triage".into()) }
    );
  }

  /// RETURNED transition 落点解析异常（落 End）→ Error（不是合法 ReworkTarget）。
  #[test]
  fn decide_advance_returned_to_end_errors() {
    let nodes = vec![start_node(), approval("approval_1", "facility_admin"), end_node()];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "end", SignalType::Approved),
      tr("approval_1", "end", SignalType::Returned), // RETURNED 指向 End → 非法 rework 目标
    ];
    let (outcome, _trace) = da_trace(&nodes, &transitions, "approval_1", SignalType::Returned, None);
    assert_eq!(outcome, AdvanceOutcome::Error);
  }

  // ============================================================
  // 缺口 C：静态拓扑 lint（§5.3.5）—— lint_definition 4 检测项
  // ============================================================

  /// 健康图（线性 start → approval → end）：lint 零 finding。
  #[test]
  fn lint_clean_linear_no_findings() {
    let (n, t) = linear_single();
    assert!(lint_definition(&n, &t).is_empty());
  }

  /// 健康并行图（split → 2 approval → join → end）：lint 零 finding（branch 均闭合到 Join）。
  #[test]
  fn lint_clean_parallel_no_findings() {
    let (n, t) = parallel_def();
    assert_eq!(lint_definition(&n, &t), Vec::<LintFinding>::new());
  }

  /// (1) dangling_edge：transition.to 引用不存在节点 "ghost_node"。
  #[test]
  fn lint_detects_dangling_edge_on_to() {
    let nodes = vec![start_node(), approval("approval_1", "facility_admin"), end_node()];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "end", SignalType::Approved),
      tr("approval_1", "ghost_node", SignalType::Returned), // 悬空 to
    ];
    let findings = lint_definition(&nodes, &transitions);
    let dangling: Vec<&LintFinding> = findings.iter().filter(|f| f.code == "dangling_edge").collect();
    assert_eq!(dangling.len(), 1, "got: {findings:?}");
    assert_eq!(dangling[0].severity, "error");
    assert_eq!(dangling[0].node_id.as_deref(), Some("ghost_node"));
  }

  /// (1) dangling_edge：transition.from 引用不存在节点。
  #[test]
  fn lint_detects_dangling_edge_on_from() {
    let nodes = vec![start_node(), approval("approval_1", "facility_admin"), end_node()];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "end", SignalType::Approved),
      tr("phantom", "end", SignalType::Approved), // 悬空 from
    ];
    let findings = lint_definition(&nodes, &transitions);
    assert!(
      findings.iter().any(|f| f.code == "dangling_edge" && f.node_id.as_deref() == Some("phantom")),
      "got: {findings:?}"
    );
  }

  /// (2) unreachable_node：某 Approval 节点未被任何 transition / branch_target 指向。
  #[test]
  fn lint_detects_unreachable_node() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      approval("orphan", "facility_director"), // 无任何入边
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "end", SignalType::Approved),
      // orphan 自己有出边（避免被当作 deadlock），但无入边 → 不可达
      tr("orphan", "end", SignalType::Approved),
    ];
    let findings = lint_definition(&nodes, &transitions);
    let unreachable: Vec<&LintFinding> = findings.iter().filter(|f| f.code == "unreachable_node").collect();
    assert_eq!(unreachable.len(), 1, "got: {findings:?}");
    assert_eq!(unreachable[0].severity, "error");
    assert_eq!(unreachable[0].node_id.as_deref(), Some("orphan"));
  }

  /// (2) start 经 RESUBMITTED 边可达首审 —— validate 用 RESUBMITTED 接 start→approval，
  /// lint 可达性必须跟随该边，不把首审误报为 unreachable。
  #[test]
  fn lint_start_resubmitted_edge_makes_first_approval_reachable() {
    let (n, t) = linear_single();
    let findings = lint_definition(&n, &t);
    assert!(!findings.iter().any(|f| f.code == "unreachable_node"), "got: {findings:?}");
  }

  /// (3) deadlock_node：EVENT_WAIT 既无 EVENT_RECEIVED 出边又无 timeout_target → 永久卡死。
  /// 这正是 validate 放行而 lint 兜底的 case —— 该图用 timeout_target=None 构造（validate
  /// 会拒绝，但 lint 是纯图工具，独立可调，覆盖「绕过 validate 的损坏快照」全图维度）。
  #[test]
  fn lint_detects_deadlock_event_wait() {
    let nodes = vec![
      start_node(),
      approval("approval_1", "facility_admin"),
      event_wait("wait_1", "evt", None), // 无 timeout_target
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_1", SignalType::Resubmitted),
      tr("approval_1", "wait_1", SignalType::Approved),
      // wait_1 无 EVENT_RECEIVED 出边、无 timeout_target → 死锁
    ];
    let findings = lint_definition(&nodes, &transitions);
    let deadlock: Vec<&LintFinding> = findings.iter().filter(|f| f.code == "deadlock_node").collect();
    assert_eq!(deadlock.len(), 1, "got: {findings:?}");
    assert_eq!(deadlock[0].severity, "error");
    assert_eq!(deadlock[0].node_id.as_deref(), Some("wait_1"));
  }

  /// (3) deadlock_node：Approval 无任何出边（非 End 却无推进路径）。
  #[test]
  fn lint_detects_deadlock_approval_no_outgoing() {
    let nodes = vec![start_node(), approval("approval_dead", "facility_admin"), end_node()];
    let transitions = vec![
      tr("start", "approval_dead", SignalType::Resubmitted),
      // approval_dead 无任何出边；end 无入边但 End 不算死锁
    ];
    let findings = lint_definition(&nodes, &transitions);
    assert!(
      findings.iter().any(|f| f.code == "deadlock_node" && f.node_id.as_deref() == Some("approval_dead")),
      "got: {findings:?}"
    );
    // End 节点无出边不应被报死锁
    assert!(!findings.iter().any(|f| f.code == "deadlock_node" && f.node_id.as_deref() == Some("end")));
  }

  /// (4) parallel_unclosed（warning）：ParallelSplit 的某 branch 不可达任何 ParallelJoin。
  /// 构造一个 branch 直接落 End（不经 Join），另一 branch 正常到 Join。
  #[test]
  fn lint_warns_parallel_branch_not_reaching_join() {
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      parallel_join("join_1"),
      end_with("end_early", Some("approved")),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved), // a 正常闭合到 Join
      tr("approval_b", "end_early", SignalType::Approved), // b 直接落 End，未达 Join
      tr("join_1", "end", SignalType::Approved),
    ];
    let findings = lint_definition(&nodes, &transitions);
    let unclosed: Vec<&LintFinding> = findings.iter().filter(|f| f.code == "parallel_unclosed").collect();
    assert_eq!(unclosed.len(), 1, "got: {findings:?}");
    assert_eq!(unclosed[0].severity, "warning");
    assert_eq!(unclosed[0].node_id.as_deref(), Some("approval_b"));
  }

  // ---- D1 dry-run：validation_error best-effort node_id 抽取 ----

  #[test]
  fn validation_error_extracts_node_id_from_quoted_token() {
    let f = LintFinding::validation_error("approval node 'approval_1' assignee_role must not be empty");
    assert_eq!(f.severity, "error");
    assert_eq!(f.code, "validation_error");
    assert_eq!(f.node_id.as_deref(), Some("approval_1"));
    assert_eq!(f.message, "approval node 'approval_1' assignee_role must not be empty");
  }

  #[test]
  fn validation_error_extracts_from_transition_message() {
    let f = LintFinding::validation_error("transition.from 'ghost' references unknown node");
    assert_eq!(f.node_id.as_deref(), Some("ghost"));
  }

  #[test]
  fn validation_error_leaves_node_id_empty_when_unquoted() {
    let f = LintFinding::validation_error("nodes must contain at least one end node");
    assert_eq!(f.node_id, None);
    assert_eq!(f.code, "validation_error");
  }

  #[test]
  fn validation_error_via_validate_definition_first_error() {
    // 缺 end 节点 → validate 早返回首个错误；包成 validation_error finding。
    let nodes = vec![start_node(), approval("approval_1", "")];
    let transitions = vec![tr("start", "approval_1", SignalType::Resubmitted)];
    let err = validate_definition(&nodes, &transitions).unwrap_err();
    let FlowError::Validation(msg) = err else { panic!("expected Validation, got {err:?}") };
    let f = LintFinding::validation_error(&msg);
    assert_eq!(f.severity, "error");
    assert_eq!(f.code, "validation_error");
  }

  // ---- 缺口 D G2 ForEach（动态扇出） ----

  fn for_each(
    id: &str,
    items_path: &str,
    branch_template: &str,
    join_target: &str,
    max_fanout: Option<i32>,
  ) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "ForEach".into(),
      context_writes: Vec::new(),
      config: NodeConfig::ForEach {
        items_path: items_path.into(),
        branch_template: branch_template.into(),
        join_target: join_target.into(),
        max_fanout,
      },
    }
  }

  /// start → approval_0 → foreach(items_path, branch_template) → join → end。
  /// branch_template 通过参数注入（Approval/Assignment/EventWait），其出边 signal 随 kind。
  fn for_each_def(
    branch_template: WorkflowNode,
    branch_to_join_signal: SignalType,
    max_fanout: Option<i32>,
  ) -> (Vec<WorkflowNode>, Vec<WorkflowTransition>) {
    let template_id = branch_template.id.clone();
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      for_each("foreach_1", "context.items", &template_id, "join_1", max_fanout),
      branch_template,
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "foreach_1", SignalType::Approved),
      tr(&template_id, "join_1", branch_to_join_signal),
      tr("join_1", "end", SignalType::Approved),
    ];
    (nodes, transitions)
  }

  #[test]
  fn validate_accepts_for_each_with_assignment_template() {
    // ForEach branch_template=Assignment（出边 COMPLETED 汇入 join）。
    let (n, t) = for_each_def(assignment("chase_one", None, Some("billing_chase")), SignalType::Completed, Some(50));
    validate_definition(&n, &t).unwrap();
  }

  #[test]
  fn validate_accepts_for_each_with_approval_template() {
    let (n, t) = for_each_def(approval("review_one", "facility_admin"), SignalType::Approved, None);
    validate_definition(&n, &t).unwrap();
  }

  #[test]
  fn validate_accepts_for_each_with_event_wait_template() {
    let (n, t) = for_each_def(event_wait("wait_one", "evt.done", Some("end")), SignalType::EventReceived, None);
    validate_definition(&n, &t).unwrap();
  }

  #[test]
  fn validate_rejects_for_each_empty_items_path() {
    let (n, t) = for_each_def_with_items("", assignment("chase_one", None, Some("q")), SignalType::Completed);
    let err = validate_definition(&n, &t).unwrap_err().to_string();
    assert!(err.contains("items_path must not be empty"), "got: {err}");
  }

  #[test]
  fn validate_rejects_for_each_bad_items_path_cel() {
    let (n, t) =
      for_each_def_with_items("context.items >>", assignment("chase_one", None, Some("q")), SignalType::Completed);
    let err = validate_definition(&n, &t).unwrap_err().to_string();
    assert!(err.contains("items_path invalid"), "got: {err}");
  }

  /// 变体：自定义 items_path（验证 items_path 校验）。
  fn for_each_def_with_items(
    items_path: &str,
    branch_template: WorkflowNode,
    branch_to_join_signal: SignalType,
  ) -> (Vec<WorkflowNode>, Vec<WorkflowTransition>) {
    let template_id = branch_template.id.clone();
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      for_each("foreach_1", items_path, &template_id, "join_1", None),
      branch_template,
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "foreach_1", SignalType::Approved),
      tr(&template_id, "join_1", branch_to_join_signal),
      tr("join_1", "end", SignalType::Approved),
    ];
    (nodes, transitions)
  }

  #[test]
  fn validate_rejects_for_each_nested_branch_template() {
    // branch_template 指向另一个 ParallelSplit（嵌套扇出）→ 拒绝。
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      for_each("foreach_1", "context.items", "split_nested", "join_1", None),
      parallel_split("split_nested", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "foreach_1", SignalType::Approved),
      tr("split_nested", "join_1", SignalType::Approved),
      tr("join_1", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("branch_template") && err.contains("no nested fan-out"), "got: {err}");
  }

  #[test]
  fn validate_rejects_for_each_join_target_not_parallel_join() {
    // join_target 指向一个 End（非 ParallelJoin）→ 拒绝。
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      for_each("foreach_1", "context.items", "chase_one", "end", None),
      assignment("chase_one", None, Some("q")),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "foreach_1", SignalType::Approved),
      tr("chase_one", "end", SignalType::Completed),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("join_target") && err.contains("must be a ParallelJoin"), "got: {err}");
  }

  #[test]
  fn validate_rejects_for_each_branch_template_without_join_edge() {
    // branch_template 没有汇入 join 的出边 → 拒绝。
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      for_each("foreach_1", "context.items", "chase_one", "join_1", None),
      assignment("chase_one", None, Some("q")),
      parallel_join("join_1"),
      end_node(),
    ];
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "foreach_1", SignalType::Approved),
      // chase_one → end（不是 join_1）→ 缺汇入 join 的边
      tr("chase_one", "end", SignalType::Completed),
      tr("join_1", "end", SignalType::Approved),
    ];
    let err = validate_definition(&nodes, &transitions).unwrap_err().to_string();
    assert!(err.contains("into join_target"), "got: {err}");
  }

  #[test]
  fn resolve_for_each_fans_out_dynamic_n_with_derived_assignment_type() {
    // context.items = [a,b,c] → 3 条 Assignment 分支（activity_type 由 branch_template kind 派生）。
    let (n, t) = for_each_def(assignment("chase_one", None, Some("billing_chase")), SignalType::Completed, Some(50));
    let ctx = serde_json::json!({"items": ["bill-1", "bill-2", "bill-3"]});
    let (out, trace) = da_trace(&n, &t, "approval_0", SignalType::Approved, Some(&ctx));
    match out {
      AdvanceOutcome::AdvanceMulti { targets } => {
        assert_eq!(targets.len(), 3);
        for (i, tgt) in targets.iter().enumerate() {
          assert_eq!(tgt.node_id, "chase_one");
          assert_eq!(tgt.activity_type, ActivityType::Assignment); // 派生，不是写死 Approval
          assert_eq!(tgt.item_index, Some(i as i32));
          assert!(tgt.item_payload.is_some());
        }
        assert_eq!(targets[1].item_payload, Some(serde_json::json!("bill-2")));
      }
      other => panic!("expected AdvanceMulti, got {other:?}"),
    }
    // trace 回传 ForEach 扇出来源（service 据此识别 ForEach + 取 join_target）。
    assert_eq!(
      trace.for_each_fan_out,
      Some(ForEachFanOut { for_each_node: "foreach_1".into(), join_target: "join_1".into() })
    );
  }

  #[test]
  fn resolve_parallel_split_has_no_for_each_fan_out_trace() {
    // ParallelSplit 来源不设 for_each_fan_out（service 据此走 PARALLEL_SPLIT_SCHEDULED 路径）。
    let (n, t) = parallel_def();
    let (out, trace) = da_trace(&n, &t, "approval_0", SignalType::Approved, None);
    assert!(matches!(out, AdvanceOutcome::AdvanceMulti { .. }));
    assert_eq!(trace.for_each_fan_out, None);
  }

  #[test]
  fn resolve_for_each_derives_approval_type_and_role() {
    let (n, t) = for_each_def(approval("review_one", "facility_director"), SignalType::Approved, None);
    let ctx = serde_json::json!({"items": [1, 2]});
    let out = da(&n, &t, "approval_0", SignalType::Approved, Some(&ctx));
    match out {
      AdvanceOutcome::AdvanceMulti { targets } => {
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].activity_type, ActivityType::Approval);
        assert_eq!(targets[0].assignee_role.as_deref(), Some("facility_director"));
      }
      other => panic!("expected AdvanceMulti, got {other:?}"),
    }
  }

  #[test]
  fn resolve_for_each_empty_array_yields_zero_targets() {
    // 空数组合法：AdvanceMulti{targets:[]}（service 视 expected=0 立即满足 join）。
    let (n, t) = for_each_def(assignment("chase_one", None, Some("q")), SignalType::Completed, None);
    let ctx = serde_json::json!({"items": []});
    let out = da(&n, &t, "approval_0", SignalType::Approved, Some(&ctx));
    assert_eq!(out, AdvanceOutcome::AdvanceMulti { targets: vec![] });
  }

  #[test]
  fn resolve_for_each_over_max_fanout_errors() {
    // 4 个元素 > max_fanout=2 → Error（service 据此回滚拒绝扇出）。
    let (n, t) = for_each_def(assignment("chase_one", None, Some("q")), SignalType::Completed, Some(2));
    let ctx = serde_json::json!({"items": [1, 2, 3, 4]});
    assert_eq!(da(&n, &t, "approval_0", SignalType::Approved, Some(&ctx)), AdvanceOutcome::Error);
    // 边界：恰好 = max_fanout 放行。
    let ctx_ok = serde_json::json!({"items": [1, 2]});
    assert!(matches!(
      da(&n, &t, "approval_0", SignalType::Approved, Some(&ctx_ok)),
      AdvanceOutcome::AdvanceMulti { .. }
    ));
  }

  #[test]
  fn resolve_for_each_non_array_items_errors() {
    // items_path 求值为非数组（对象/标量）→ Error（for_each_items_invalid）。
    let (n, t) = for_each_def(assignment("chase_one", None, Some("q")), SignalType::Completed, None);
    let ctx = serde_json::json!({"items": {"not": "array"}});
    assert_eq!(da(&n, &t, "approval_0", SignalType::Approved, Some(&ctx)), AdvanceOutcome::Error);
    let ctx_scalar = serde_json::json!({"items": 42});
    assert_eq!(da(&n, &t, "approval_0", SignalType::Approved, Some(&ctx_scalar)), AdvanceOutcome::Error);
  }

  #[test]
  fn parallel_join_incoming_signal_relaxed_accepts_completed_and_event_received() {
    // 一条 Assignment 分支（COMPLETED）+ 一条 EventWait 分支（EVENT_RECEIVED）汇入同一 Join：
    // 放宽后 incoming signal 集合命中（不再 Approval-only），validate 通过。
    let nodes = vec![
      start_node(),
      approval("approval_0", "facility_admin"),
      parallel_split("split_1", vec!["approval_a", "approval_b"]),
      approval("approval_a", "facility_admin"),
      approval("approval_b", "facility_director"),
      assignment("assign_c", None, Some("q")),
      parallel_join("join_1"),
      end_node(),
    ];
    // split 仅分叉 a/b（Approval-only，符合 G1 split 契约），但 join 还额外接受 assign_c 的 COMPLETED 入边。
    let transitions = vec![
      tr("start", "approval_0", SignalType::Resubmitted),
      tr("approval_0", "split_1", SignalType::Approved),
      tr("approval_a", "join_1", SignalType::Approved),
      tr("approval_b", "join_1", SignalType::Approved),
      // 放宽：COMPLETED 入边计入 join incoming
      tr("assign_c", "join_1", SignalType::Completed),
      tr("join_1", "end", SignalType::Approved),
    ];
    validate_definition(&nodes, &transitions).unwrap();
  }

  #[test]
  fn parallel_join_fed_by_for_each_exempt_from_two_incoming_requirement() {
    // ForEach-fed join 静态只有 1 条 branch_template→join 入边，豁免 ≥2 incoming 要求。
    let (n, t) = for_each_def(assignment("chase_one", None, Some("q")), SignalType::Completed, None);
    // 该 join 只有 1 条静态 incoming（chase_one →COMPLETED→ join_1），仍应 validate 通过。
    validate_definition(&n, &t).unwrap();
  }
}
