//! Event-log → 投影重放重建（projection rebuild）。
//!
//! [`fold_events`] 是纯函数 reducer：按 `seq` 升序消费实例的全部 [`EventRecord`]，结合不可变
//! [`DefinitionSnapshot`] 重建 canonical 投影（`workflow_instances` / `activity_instances` 的结构
//! 状态）。这是 hetuflow「执行历史可追溯 + 投影可重建」能力的地基。
//!
//! 设计真相源：`docs/designs/workflow/hetuflow-projection-traceability.md`。
//!
//! ## Canonical 边界（可由 event-log bit-exact 重建）
//! - instance: `status` / `current_activity_id` / `result` / `started_at` / `completed_at`
//! - activity: `id` / `activity_definition_id` / `activity_type` / `status` / `assignee_role` /
//!   `assignee_id` / `result` / `review_notes` / `reviewed_by` / `reviewed_at` / `round`
//!
//! ## 排除 canonical（不在 event-log，漂移核验时另行交叉核对）
//! - `instance.side_effects_executed` —— 由 outbox 投递态派生（`mark_side_effects_executed` 仅
//!   UPDATE 投影、不 append 事件）。
//! - `created_at` / `updated_at` —— bookkeeping 时间戳。
//!
//! ## 关键不变式
//! - `activity_type` 一律从 `snapshot` 节点 `kind` 派生：部分 `ACTIVITY_SCHEDULED` /
//!   `RETURNED_TO_REWORK` payload 不带该字段（start / rework / resubmit 路径），不可依赖 payload。
//! - 并行 / ForEach 扇出后实例无单一 `current_activity_id`（落 NULL）；只有线性 advance 才指向新活动。
//! - `workflow.cancelled.v1` 当前无事件生产者（service 层未实现 workflow 取消）；若将来新增取消能力，
//!   MUST 同步 append 取消事件，否则 cancelled 实例无法由 event-log 重建（前向护栏）。

use chrono::{DateTime, Utc};
use hetuflow_core::{
  ActivityInstance, ActivityStatus, ActivityType, DefinitionSnapshot, EventRecord, NodeKind, WorkflowInstance,
  WorkflowResult, WorkflowStatus, event_type,
};

/// notification 投递成功完成活动时 `reviewed_by` 落 nil uuid（`complete_activity_cas` reviewed_by=Uuid::nil）。
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

/// 重建出的实例投影（canonical 结构子集）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayedInstance {
  pub status: WorkflowStatus,
  pub current_activity_id: Option<String>,
  pub result: Option<WorkflowResult>,
  pub started_at: Option<DateTime<Utc>>,
  pub completed_at: Option<DateTime<Utc>>,
}

/// 重建出的活动投影（canonical 结构子集）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayedActivity {
  pub id: String,
  pub activity_definition_id: String,
  pub activity_type: ActivityType,
  pub status: ActivityStatus,
  pub assignee_role: Option<String>,
  pub assignee_id: Option<String>,
  pub result: Option<String>,
  pub review_notes: Option<String>,
  pub reviewed_by: Option<String>,
  pub reviewed_at: Option<DateTime<Utc>>,
  pub round: i32,
}

/// 一次重放重建的完整投影（按事件顺序，activity 以创建序排列）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayedProjection {
  pub instance: ReplayedInstance,
  pub activities: Vec<ReplayedActivity>,
}

/// 节点 `kind` → 实例化活动的 `ActivityType`（镜像 `service.rs` 线性 advance 的派生逻辑）。
///
/// 仅会被实例化为 activity 行的 kind 出现（Approval / Notification / EventWait / Assignment）；
/// 其余 kind（Start / End / Condition / Merge / ParallelSplit / ParallelJoin / ForEach / ...）按
/// service 现状归 `Approval` 兜底，但这些 kind 不会被 `ACTIVITY_SCHEDULED` 引用，故兜底不实际命中。
pub fn instantiated_activity_type(kind: NodeKind) -> ActivityType {
  match kind {
    NodeKind::EventWait => ActivityType::EventWait,
    NodeKind::Notification => ActivityType::Notification,
    NodeKind::Assignment => ActivityType::Assignment,
    _ => ActivityType::Approval,
  }
}

fn is_terminal(status: WorkflowStatus) -> bool {
  matches!(status, WorkflowStatus::Completed | WorkflowStatus::Errored | WorkflowStatus::Cancelled)
}

fn pstr(payload: &serde_json::Value, key: &str) -> Option<String> {
  payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn pi32(payload: &serde_json::Value, key: &str) -> Option<i32> {
  payload.get(key).and_then(serde_json::Value::as_i64).map(|n| n as i32)
}

fn activity_type_for(snapshot: &DefinitionSnapshot, def_id: &str) -> ActivityType {
  snapshot
    .nodes
    .iter()
    .find(|n| n.id == def_id)
    .map(|n| instantiated_activity_type(n.kind()))
    .unwrap_or(ActivityType::Approval)
}

/// 纯函数 reducer：从不可变快照 + 有序事件流重建 canonical 投影。
///
/// `events` MUST 已按 `seq` 升序（`load_events` 已保证 `ORDER BY seq`）。未知 / 未来 `event_type`
/// 被忽略（向前兼容）。本函数无 I/O、不读 DB、确定性，可独立单测 + 在 dry-run 漂移核验中复用。
pub fn fold_events(snapshot: &DefinitionSnapshot, events: &[EventRecord]) -> ReplayedProjection {
  let mut instance = ReplayedInstance {
    status: WorkflowStatus::Pending,
    current_activity_id: None,
    result: None,
    started_at: None,
    completed_at: None,
  };
  let mut activities: Vec<ReplayedActivity> = Vec::new();

  for ev in events {
    let p = &ev.payload;
    match ev.event_type.as_str() {
      // 实例创建即 active（insert_instance_on_conflict 落 status='active'）。
      event_type::STARTED => {
        instance.status = WorkflowStatus::Active;
        instance.started_at = Some(ev.created_at);
      }

      // 新建 active 活动；线性 advance → current 指向它；并行 / ForEach 分支 → current 落 NULL。
      event_type::ACTIVITY_SCHEDULED => {
        let Some(id) = pstr(p, "activity_id") else { continue };
        let def_id = pstr(p, "activity_definition_id").unwrap_or_default();
        let round = pi32(p, "round").unwrap_or(1);
        let via = p.get("via").and_then(|v| v.as_str());
        activities.push(ReplayedActivity {
          id: id.clone(),
          activity_definition_id: def_id.clone(),
          activity_type: activity_type_for(snapshot, &def_id),
          status: ActivityStatus::Active,
          assignee_role: pstr(p, "assignee_role"),
          assignee_id: None,
          result: None,
          review_notes: None,
          reviewed_by: None,
          reviewed_at: None,
          round,
        });
        if matches!(via, Some("for_each") | Some("parallel_split")) {
          instance.current_activity_id = None;
        } else {
          instance.current_activity_id = Some(id);
        }
        if !is_terminal(instance.status) {
          instance.status = WorkflowStatus::Active;
        }
      }

      // 两形态：claim/reassign 仅改 assignee_id（活动不完成）；其余记 reviewer/notes（完成由后续 ACTIVITY_COMPLETED 落）。
      event_type::SIGNAL_RECEIVED => {
        let signal = pstr(p, "signal_type").unwrap_or_default();
        if let Some(tid) = pstr(p, "target_activity_id")
          && let Some(a) = activities.iter_mut().find(|a| a.id == tid)
        {
          if signal == "claimed" || signal == "reassigned" {
            a.assignee_id = pstr(p, "assignee_id");
          } else {
            a.reviewed_by = pstr(p, "reviewer_id");
            a.review_notes = pstr(p, "review_notes");
            a.reviewed_at = Some(ev.created_at);
          }
        }
      }

      event_type::ACTIVITY_COMPLETED => {
        let id = pstr(p, "activity_id").unwrap_or_default();
        let from_notification = p.get("source").and_then(|v| v.as_str()) == Some("notification_outbox");
        if let Some(a) = activities.iter_mut().find(|a| a.id == id) {
          a.status = ActivityStatus::Completed;
          a.result = pstr(p, "result");
          // notification 投递完成无前置 SIGNAL_RECEIVED：reviewer 落 nil、reviewed_at 取事件时间。
          if from_notification {
            a.reviewed_by = Some(NIL_UUID.to_string());
            a.reviewed_at = Some(ev.created_at);
          }
        }
      }

      // 带 activity_id（timeout）→ 跳该活动；不带（fail-fast / join-satisfied 兄弟）→ 跳全部仍 active。
      event_type::ACTIVITY_SKIPPED => {
        if let Some(id) = pstr(p, "activity_id") {
          if let Some(a) = activities.iter_mut().find(|a| a.id == id) {
            a.status = ActivityStatus::Skipped;
          }
        } else {
          for a in activities.iter_mut().filter(|a| matches!(a.status, ActivityStatus::Active)) {
            a.status = ActivityStatus::Skipped;
          }
        }
      }

      event_type::COMPLETED => {
        instance.status = WorkflowStatus::Completed;
        instance.result = pstr(p, "result").and_then(|s| WorkflowResult::from_db(&s));
        instance.completed_at = Some(ev.created_at);
        instance.current_activity_id = None;
      }

      // error_workflow 只改 status（不设 completed_at / result、不清 current）。
      event_type::ERRORED => {
        instance.status = WorkflowStatus::Errored;
      }

      // 退回返工：实例置 returned_waiting，并新建 round+1 的返工目标活动（current 指向它）。
      // payload 的 activity_id / assignee_role 由可推导审计补齐（缺则降级用占位 id）。
      event_type::RETURNED_TO_REWORK => {
        instance.status = WorkflowStatus::ReturnedWaiting;
        let def_id = pstr(p, "rework_target").unwrap_or_default();
        let round = pi32(p, "round").unwrap_or(1);
        let id = pstr(p, "activity_id").unwrap_or_else(|| format!("<rework:{def_id}:{round}>"));
        activities.push(ReplayedActivity {
          id: id.clone(),
          activity_definition_id: def_id.clone(),
          activity_type: activity_type_for(snapshot, &def_id),
          status: ActivityStatus::Active,
          assignee_role: pstr(p, "assignee_role"),
          assignee_id: None,
          result: None,
          review_notes: None,
          reviewed_by: None,
          reviewed_at: None,
          round,
        });
        instance.current_activity_id = Some(id);
      }

      // 返工目标通过 → 重入主链（后续 ACTIVITY_SCHEDULED 再改 current）。
      event_type::REWORK_RESUMED => {
        instance.status = WorkflowStatus::Active;
      }

      // resubmit reactivate_workflow：completed+returned → active，清 result / completed_at。
      event_type::REACTIVATED => {
        instance.status = WorkflowStatus::Active;
        instance.result = None;
        instance.completed_at = None;
      }

      // 审批升级仅换 owner（活动保持 active，不完成 / 不推进）。
      event_type::APPROVAL_ESCALATED => {
        if let Some(id) = pstr(p, "activity_id")
          && let Some(to_role) = pstr(p, "to_role")
          && let Some(a) = activities.iter_mut().find(|a| a.id == id)
        {
          a.assignee_role = Some(to_role);
        }
      }

      // 扇出标记：统一清 current（分支 ACTIVITY_SCHEDULED 亦置 None，幂等）。
      event_type::PARALLEL_SPLIT_SCHEDULED | event_type::FOR_EACH_FANNED_OUT => {
        instance.current_activity_id = None;
      }

      // 纯事实事件（marker / 复述 / 投递失败 / timer 触发），投影变更由相邻的 CAS 事件承载：
      // MERGE_PASSED / BRANCH_REACHED_JOIN / PARALLEL_JOIN_SATISFIED / ACTIVITY_FAILED / TIMER_FIRED。
      _ => {}
    }
  }

  ReplayedProjection { instance, activities }
}

/// 已完成 activity 时间线（公共读路径；rework / compensation 后续可复用，避免两套回放逻辑）。
pub fn replay_completed_activities(proj: &ReplayedProjection) -> Vec<&ReplayedActivity> {
  proj.activities.iter().filter(|a| matches!(a.status, ActivityStatus::Completed)).collect()
}

/// 比对重建投影 vs live 投影的 canonical 字段，返回人类可读差异行（空 Vec = 一致 = 投影可由 event-log 重建）。
///
/// 排除 canonical 之外：`side_effects_executed`（outbox 派生）与时间戳（advisory，事件时间 ≈ 行 now() 但非 bit-exact）。
/// activity 按 `id` 配对（`id` 来自 `activity.scheduled.v1` payload，可重建）。
pub fn diff_projection(
  replayed: &ReplayedProjection,
  live_instance: &WorkflowInstance,
  live_activities: &[ActivityInstance],
) -> Vec<String> {
  let mut diffs = Vec::new();
  let (ri, li) = (&replayed.instance, live_instance);
  if ri.status != li.status {
    diffs.push(format!("instance.status: replay={:?} live={:?}", ri.status, li.status));
  }
  if ri.current_activity_id != li.current_activity_id {
    diffs.push(format!(
      "instance.current_activity_id: replay={:?} live={:?}",
      ri.current_activity_id, li.current_activity_id
    ));
  }
  if ri.result != li.result {
    diffs.push(format!("instance.result: replay={:?} live={:?}", ri.result, li.result));
  }

  use std::collections::{BTreeMap, BTreeSet};
  let live_by_id: BTreeMap<&str, &ActivityInstance> = live_activities.iter().map(|a| (a.id.as_str(), a)).collect();
  let replay_ids: BTreeSet<&str> = replayed.activities.iter().map(|a| a.id.as_str()).collect();
  for ra in &replayed.activities {
    let Some(la) = live_by_id.get(ra.id.as_str()) else {
      diffs.push(format!("activity[{}]: replay 有但 live 缺", ra.id));
      continue;
    };
    if ra.status != la.status {
      diffs.push(format!("activity[{}].status: replay={:?} live={:?}", ra.id, ra.status, la.status));
    }
    if ra.result != la.result {
      diffs.push(format!("activity[{}].result: replay={:?} live={:?}", ra.id, ra.result, la.result));
    }
    if ra.round != la.round {
      diffs.push(format!("activity[{}].round: replay={} live={}", ra.id, ra.round, la.round));
    }
    if ra.assignee_role != la.assignee_role {
      diffs
        .push(format!("activity[{}].assignee_role: replay={:?} live={:?}", ra.id, ra.assignee_role, la.assignee_role));
    }
    if ra.assignee_id != la.assignee_id {
      diffs.push(format!("activity[{}].assignee_id: replay={:?} live={:?}", ra.id, ra.assignee_id, la.assignee_id));
    }
    if ra.review_notes != la.review_notes {
      diffs.push(format!("activity[{}].review_notes: replay={:?} live={:?}", ra.id, ra.review_notes, la.review_notes));
    }
    if ra.reviewed_by != la.reviewed_by {
      diffs.push(format!("activity[{}].reviewed_by: replay={:?} live={:?}", ra.id, ra.reviewed_by, la.reviewed_by));
    }
  }
  for la in live_activities {
    if !replay_ids.contains(la.id.as_str()) {
      diffs.push(format!("activity[{}]: live 有但 replay 缺", la.id));
    }
  }
  diffs
}

#[cfg(test)]
mod tests {
  use super::*;
  use hetuflow_core::{NotificationChannelPolicy, NotificationConfig, WorkflowNode};
  use serde_json::json;

  fn ts(s: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(s, 0).unwrap()
  }

  fn ev(seq: i64, et: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord { seq, event_type: et.to_string(), payload, created_at: ts(seq) }
  }

  fn snap(nodes: Vec<WorkflowNode>) -> DefinitionSnapshot {
    DefinitionSnapshot { flow_type: "test".into(), version: 1, nodes, transitions: vec![], max_round: 0 }
  }

  fn approval(id: &str, role: &str) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "审批".into(),
      context_writes: Vec::new(),
      config: hetuflow_core::NodeConfig::Approval {
        assignee_role: Some(role.into()),
        sla_seconds: None,
        escalation_seconds: None,
        escalation_target_role: None,
      },
    }
  }

  fn notification(id: &str) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: "通知".into(),
      context_writes: Vec::new(),
      config: hetuflow_core::NodeConfig::Notification(NotificationConfig {
        template_code: Some("t".into()),
        recipient_selector: None,
        channel_policy: NotificationChannelPolicy { notification_types: vec![] },
        template_args: Default::default(),
        reference_type: None,
        purpose: None,
        visibility: None,
      }),
    }
  }

  #[test]
  fn fold_linear_approve_complete() {
    let s = snap(vec![approval("a1", "nurse")]);
    let evs = vec![
      ev(1, event_type::STARTED, json!({"definition_id":"d","business_key":"bk","business_type":"bt"})),
      ev(
        2,
        event_type::ACTIVITY_SCHEDULED,
        json!({"activity_id":"act-1","activity_definition_id":"a1","assignee_role":"nurse","round":1}),
      ),
      ev(
        3,
        event_type::SIGNAL_RECEIVED,
        json!({"signal_type":"approved","reviewer_id":"u-1","review_notes":"ok","target_activity_id":"act-1"}),
      ),
      ev(
        4,
        event_type::ACTIVITY_COMPLETED,
        json!({"activity_id":"act-1","activity_definition_id":"a1","result":"approved","round":1}),
      ),
      ev(5, event_type::COMPLETED, json!({"result":"approved"})),
    ];
    let p = fold_events(&s, &evs);
    assert_eq!(p.instance.status, WorkflowStatus::Completed);
    assert_eq!(p.instance.result, WorkflowResult::from_db("approved"));
    assert_eq!(p.instance.current_activity_id, None);
    assert_eq!(p.instance.started_at, Some(ts(1)));
    assert_eq!(p.instance.completed_at, Some(ts(5)));
    assert_eq!(p.activities.len(), 1);
    let a = &p.activities[0];
    assert_eq!(a.id, "act-1");
    assert_eq!(a.activity_type, ActivityType::Approval);
    assert_eq!(a.status, ActivityStatus::Completed);
    assert_eq!(a.result.as_deref(), Some("approved"));
    assert_eq!(a.reviewed_by.as_deref(), Some("u-1"));
    assert_eq!(a.review_notes.as_deref(), Some("ok"));
    assert_eq!(a.reviewed_at, Some(ts(3)));
    assert_eq!(replay_completed_activities(&p).len(), 1);
  }

  #[test]
  fn fold_rework_returned_waiting() {
    let s = snap(vec![approval("a1", "nurse"), approval("a0", "lead")]);
    let evs = vec![
      ev(1, event_type::STARTED, json!({})),
      ev(2, event_type::ACTIVITY_SCHEDULED, json!({"activity_id":"act-1","activity_definition_id":"a1","round":1})),
      ev(
        3,
        event_type::SIGNAL_RECEIVED,
        json!({"signal_type":"returned","reviewer_id":"u-1","review_notes":"redo","target_activity_id":"act-1"}),
      ),
      ev(
        4,
        event_type::ACTIVITY_COMPLETED,
        json!({"activity_id":"act-1","activity_definition_id":"a1","result":"returned","round":1}),
      ),
      // payload 含 activity_id / assignee_role（可推导审计补齐后）。
      ev(
        5,
        event_type::RETURNED_TO_REWORK,
        json!({"activity_id":"act-2","from_node":"a1","rework_target":"a0","assignee_role":"lead","round":2}),
      ),
    ];
    let p = fold_events(&s, &evs);
    assert_eq!(p.instance.status, WorkflowStatus::ReturnedWaiting);
    assert_eq!(p.instance.current_activity_id.as_deref(), Some("act-2"));
    assert_eq!(p.activities.len(), 2);
    let rework = p.activities.iter().find(|a| a.id == "act-2").unwrap();
    assert_eq!(rework.activity_definition_id, "a0");
    assert_eq!(rework.round, 2);
    assert_eq!(rework.status, ActivityStatus::Active);
    assert_eq!(rework.assignee_role.as_deref(), Some("lead"));
  }

  #[test]
  fn fold_parallel_split_clears_current() {
    let s = snap(vec![approval("a1", "nurse"), approval("b1", "r1"), approval("b2", "r2")]);
    let evs = vec![
      ev(1, event_type::STARTED, json!({})),
      ev(2, event_type::ACTIVITY_SCHEDULED, json!({"activity_id":"act-1","activity_definition_id":"a1","round":1})),
      ev(3, event_type::SIGNAL_RECEIVED, json!({"signal_type":"approved","target_activity_id":"act-1"})),
      ev(
        4,
        event_type::ACTIVITY_COMPLETED,
        json!({"activity_id":"act-1","activity_definition_id":"a1","result":"approved","round":1}),
      ),
      ev(5, event_type::PARALLEL_SPLIT_SCHEDULED, json!({"from_node":"a1","branch_targets":["b1","b2"],"round":1})),
      ev(
        6,
        event_type::ACTIVITY_SCHEDULED,
        json!({"activity_id":"act-b1","activity_definition_id":"b1","round":1,"via":"parallel_split"}),
      ),
      ev(
        7,
        event_type::ACTIVITY_SCHEDULED,
        json!({"activity_id":"act-b2","activity_definition_id":"b2","round":1,"via":"parallel_split"}),
      ),
    ];
    let p = fold_events(&s, &evs);
    assert_eq!(p.instance.current_activity_id, None);
    assert_eq!(p.instance.status, WorkflowStatus::Active);
    let active: Vec<_> = p.activities.iter().filter(|a| a.status == ActivityStatus::Active).collect();
    assert_eq!(active.len(), 2);
  }

  #[test]
  fn fold_errored_keeps_minimal() {
    let s = snap(vec![approval("a1", "nurse")]);
    let evs = vec![
      ev(1, event_type::STARTED, json!({})),
      ev(2, event_type::ACTIVITY_SCHEDULED, json!({"activity_id":"act-1","activity_definition_id":"a1","round":1})),
      ev(3, event_type::ERRORED, json!({"reason":"no matching transition","from_node":"a1"})),
    ];
    let p = fold_events(&s, &evs);
    assert_eq!(p.instance.status, WorkflowStatus::Errored);
    assert_eq!(p.instance.completed_at, None);
    assert_eq!(p.instance.result, None);
    // error_workflow 不清 current_activity_id。
    assert_eq!(p.instance.current_activity_id.as_deref(), Some("act-1"));
  }

  #[test]
  fn fold_notification_completion_nil_reviewer() {
    let s = snap(vec![notification("n1")]);
    let evs = vec![
      ev(1, event_type::STARTED, json!({})),
      ev(
        2,
        event_type::ACTIVITY_SCHEDULED,
        json!({"activity_id":"act-1","activity_definition_id":"n1","activity_type":"notification","round":1,"via":"notification"}),
      ),
      ev(
        3,
        event_type::ACTIVITY_COMPLETED,
        json!({"activity_id":"act-1","activity_definition_id":"n1","result":"completed","round":1,"source":"notification_outbox"}),
      ),
    ];
    let p = fold_events(&s, &evs);
    let a = &p.activities[0];
    assert_eq!(a.activity_type, ActivityType::Notification);
    assert_eq!(a.status, ActivityStatus::Completed);
    assert_eq!(a.reviewed_by.as_deref(), Some(NIL_UUID));
    assert_eq!(a.reviewed_at, Some(ts(3)));
  }

  #[test]
  fn fold_sibling_skip_on_complete() {
    let s = snap(vec![approval("b1", "r1"), approval("b2", "r2")]);
    let evs = vec![
      ev(
        1,
        event_type::ACTIVITY_SCHEDULED,
        json!({"activity_id":"act-b1","activity_definition_id":"b1","round":1,"via":"parallel_split"}),
      ),
      ev(
        2,
        event_type::ACTIVITY_SCHEDULED,
        json!({"activity_id":"act-b2","activity_definition_id":"b2","round":1,"via":"parallel_split"}),
      ),
      ev(3, event_type::SIGNAL_RECEIVED, json!({"signal_type":"rejected","target_activity_id":"act-b1"})),
      ev(
        4,
        event_type::ACTIVITY_COMPLETED,
        json!({"activity_id":"act-b1","activity_definition_id":"b1","result":"rejected","round":1}),
      ),
      ev(5, event_type::ACTIVITY_SKIPPED, json!({"reason":"fail_fast_sibling","skipped_count":1})),
      ev(6, event_type::COMPLETED, json!({"result":"rejected"})),
    ];
    let p = fold_events(&s, &evs);
    let b1 = p.activities.iter().find(|a| a.id == "act-b1").unwrap();
    let b2 = p.activities.iter().find(|a| a.id == "act-b2").unwrap();
    assert_eq!(b1.status, ActivityStatus::Completed);
    assert_eq!(b2.status, ActivityStatus::Skipped);
    assert_eq!(p.instance.status, WorkflowStatus::Completed);
  }

  #[test]
  fn fold_claim_sets_assignee_only() {
    let s = snap(vec![approval("a1", "nurse")]);
    let evs = vec![
      ev(1, event_type::STARTED, json!({})),
      ev(2, event_type::ACTIVITY_SCHEDULED, json!({"activity_id":"act-1","activity_definition_id":"a1","round":1})),
      ev(
        3,
        event_type::SIGNAL_RECEIVED,
        json!({"signal_type":"claimed","target_activity_id":"act-1","assignee_id":"u-9"}),
      ),
    ];
    let p = fold_events(&s, &evs);
    let a = &p.activities[0];
    assert_eq!(a.assignee_id.as_deref(), Some("u-9"));
    assert_eq!(a.status, ActivityStatus::Active); // claim 不完成活动
  }

  #[test]
  fn instantiated_type_mapping() {
    assert_eq!(instantiated_activity_type(NodeKind::EventWait), ActivityType::EventWait);
    assert_eq!(instantiated_activity_type(NodeKind::Notification), ActivityType::Notification);
    assert_eq!(instantiated_activity_type(NodeKind::Assignment), ActivityType::Assignment);
    assert_eq!(instantiated_activity_type(NodeKind::Approval), ActivityType::Approval);
    assert_eq!(instantiated_activity_type(NodeKind::Condition), ActivityType::Approval);
  }

  #[test]
  fn fold_ignores_unknown_event() {
    let s = snap(vec![approval("a1", "nurse")]);
    let evs = vec![ev(1, event_type::STARTED, json!({})), ev(2, "workflow.future_capability.v9", json!({"x":1}))];
    let p = fold_events(&s, &evs);
    assert_eq!(p.instance.status, WorkflowStatus::Active);
    assert!(p.activities.is_empty());
  }

  fn live_instance(status: WorkflowStatus, result: Option<&str>) -> WorkflowInstance {
    WorkflowInstance {
      id: "wf-1".into(),
      tenant_id: "t".into(),
      facility_id: "f".into(),
      reference_no: "WF-20260625-0001".into(),
      workflow_definition_id: "d".into(),
      business_key: "bk".into(),
      business_type: "bt".into(),
      status,
      current_activity_id: None,
      result: result.and_then(WorkflowResult::from_db),
      side_effects_executed: false,
      context: None,
      started_at: Some(ts(1)),
      completed_at: Some(ts(5)),
      created_at: ts(1),
      updated_at: ts(5),
    }
  }

  #[test]
  fn diff_projection_consistent_and_drift() {
    let s = snap(vec![approval("a1", "nurse")]);
    let evs = vec![
      ev(1, event_type::STARTED, json!({})),
      ev(2, event_type::ACTIVITY_SCHEDULED, json!({"activity_id":"act-1","activity_definition_id":"a1","round":1})),
      ev(
        3,
        event_type::SIGNAL_RECEIVED,
        json!({"signal_type":"approved","reviewer_id":"u-1","target_activity_id":"act-1"}),
      ),
      ev(
        4,
        event_type::ACTIVITY_COMPLETED,
        json!({"activity_id":"act-1","activity_definition_id":"a1","result":"approved","round":1}),
      ),
      ev(5, event_type::COMPLETED, json!({"result":"approved"})),
    ];
    let p = fold_events(&s, &evs);
    let live_act = ActivityInstance {
      id: "act-1".into(),
      tenant_id: "t".into(),
      workflow_instance_id: "wf-1".into(),
      activity_definition_id: "a1".into(),
      activity_type: ActivityType::Approval,
      status: ActivityStatus::Completed,
      assignee_role: None,
      assignee_id: None,
      result: Some("approved".into()),
      review_notes: None,
      reviewed_by: Some("u-1".into()),
      reviewed_at: Some(ts(3)),
      round: 1,
    };
    // 一致（时间戳不参与 diff）→ 空。
    assert!(
      diff_projection(&p, &live_instance(WorkflowStatus::Completed, Some("approved")), std::slice::from_ref(&live_act))
        .is_empty()
    );
    // 注入实例态漂移。
    let d1 =
      diff_projection(&p, &live_instance(WorkflowStatus::Active, Some("approved")), std::slice::from_ref(&live_act));
    assert!(d1.iter().any(|d| d.contains("instance.status")));
    // 注入活动态漂移。
    let mut drifted_act = live_act.clone();
    drifted_act.status = ActivityStatus::Active;
    let d2 = diff_projection(&p, &live_instance(WorkflowStatus::Completed, Some("approved")), &[drifted_act]);
    assert!(d2.iter().any(|d| d.contains("activity[act-1].status")));
  }
}
