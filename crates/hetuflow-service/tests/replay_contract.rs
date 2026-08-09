//! Replay contract: the event payloads `WorkflowService` writes MUST be enough for
//! `hetuflow_runtime::replay::fold_events` to rebuild the canonical projection.
//!
//! These tests are the pure half of acceptance "projection is rebuildable from the event log":
//! they pin the payload **shape** without a database. The other half — that the live projection
//! rows actually match the rebuild — needs real Postgres and lives in the consuming application's
//! API tests (`tests/suites/<consumer>-workflow.test.ts` in the consumer repo).
//!
//! If a payload key here drifts from `service.rs`, this test keeps passing while production
//! rebuild silently degrades — so every event constructed below mirrors a literal `json!` in
//! `service.rs`, and the two MUST be edited together.

use chrono::{DateTime, TimeZone, Utc};
use hetuflow_core::SignalType;
use hetuflow_core::{
  ActivityStatus, ActivityType, DefinitionSnapshot, EventRecord, JoinStrategy, NodeConfig, WorkflowNode,
  WorkflowResult, WorkflowStatus, WorkflowTransition, event_type,
};
use hetuflow_runtime::replay::fold_events;
use hetuflow_runtime::{lint_definition, validate_definition};
use serde_json::json;

const A1: &str = "11111111-1111-1111-1111-111111111111";
const A2: &str = "22222222-2222-2222-2222-222222222222";
const A3: &str = "33333333-3333-3333-3333-333333333333";

fn at(secs: i64) -> DateTime<Utc> {
  Utc.timestamp_opt(1_800_000_000 + secs, 0).single().expect("valid timestamp")
}

fn ev(seq: i64, event_type: &str, payload: serde_json::Value) -> EventRecord {
  EventRecord { seq, event_type: event_type.to_string(), payload, created_at: at(seq) }
}

fn approval(id: &str, role: &str) -> WorkflowNode {
  WorkflowNode {
    id: id.into(),
    name: id.into(),
    context_writes: Vec::new(),
    config: NodeConfig::Approval {
      assignee_role: Some(role.into()),
      sla_seconds: None,
      escalation_seconds: None,
      escalation_target_role: None,
    },
  }
}

fn end(id: &str, result: &str) -> WorkflowNode {
  WorkflowNode {
    id: id.into(),
    name: id.into(),
    context_writes: Vec::new(),
    config: NodeConfig::End { result_code: Some(result.into()) },
  }
}

fn start(id: &str) -> WorkflowNode {
  WorkflowNode { id: id.into(), name: id.into(), context_writes: Vec::new(), config: NodeConfig::Start }
}

fn t(from: &str, to: &str, on: SignalType) -> WorkflowTransition {
  WorkflowTransition { from: from.into(), to: to.into(), on_signal: on }
}

fn minimal_snapshot() -> DefinitionSnapshot {
  DefinitionSnapshot {
    flow_type: "test.minimal".into(),
    version: 1,
    max_round: 3,
    nodes: vec![start("start"), approval("review", "head_nurse"), end("ok", "approved"), end("no", "rejected")],
    transitions: vec![
      t("start", "review", SignalType::Completed),
      t("review", "ok", SignalType::Approved),
      t("review", "no", SignalType::Rejected),
    ],
  }
}

#[test]
fn minimal_definition_is_valid_and_lint_clean() {
  let snapshot = minimal_snapshot();
  validate_definition(&snapshot.nodes, &snapshot.transitions).expect("validates");
  let findings = lint_definition(&snapshot.nodes, &snapshot.transitions);
  assert!(findings.iter().all(|f| f.severity != "error"), "{findings:?}");
}

/// The exact event sequence `start` + `signal(APPROVED)` produce on the minimal definition.
#[test]
fn approval_happy_path_rebuilds_the_projection() {
  let snapshot = minimal_snapshot();
  let events = vec![
    ev(
      1,
      event_type::STARTED,
      json!({
        "flow_type": "test.minimal", "version": 1, "definition_hash": "deadbeef",
        "business_type": "demo", "business_key": "k-1", "context": null, "terminal_callback": null
      }),
    ),
    ev(
      2,
      event_type::ACTIVITY_SCHEDULED,
      json!({
        "activity_id": A1, "activity_definition_id": "review", "activity_type": "approval",
        "round": 1, "assignee_role": "head_nurse"
      }),
    ),
    ev(
      3,
      event_type::SIGNAL_RECEIVED,
      json!({
        "signal_type": "approved", "target_activity_id": A1, "reviewer_id": "42", "review_notes": "looks good"
      }),
    ),
    ev(4, event_type::ACTIVITY_COMPLETED, json!({ "activity_id": A1, "result": "approved" })),
    ev(5, event_type::COMPLETED, json!({ "result": "approved" })),
  ];

  let projection = fold_events(&snapshot, &events);
  assert_eq!(projection.instance.status, WorkflowStatus::Completed);
  assert_eq!(projection.instance.result, Some(WorkflowResult::Approved));
  assert_eq!(projection.instance.current_activity_id, None);
  assert_eq!(projection.instance.started_at, Some(at(1)));
  assert_eq!(projection.instance.completed_at, Some(at(5)));

  assert_eq!(projection.activities.len(), 1);
  let activity = &projection.activities[0];
  assert_eq!(activity.id, A1);
  assert_eq!(activity.activity_definition_id, "review");
  assert_eq!(activity.activity_type, ActivityType::Approval);
  assert_eq!(activity.status, ActivityStatus::Completed);
  assert_eq!(activity.assignee_role.as_deref(), Some("head_nurse"));
  assert_eq!(activity.result.as_deref(), Some("approved"));
  assert_eq!(activity.reviewed_by.as_deref(), Some("42"));
  assert_eq!(activity.review_notes.as_deref(), Some("looks good"));
  assert_eq!(activity.reviewed_at, Some(at(3)));
  assert_eq!(activity.round, 1);
}

/// Precise rework: the `returned_waiting` middle state plus a round-2 activity at the target.
#[test]
fn precise_rework_rebuilds_middle_state_and_second_round() {
  let mut snapshot = minimal_snapshot();
  snapshot.nodes.insert(1, approval("submit", "nurse"));
  snapshot.transitions = vec![
    t("start", "submit", SignalType::Completed),
    t("submit", "review", SignalType::Approved),
    t("review", "ok", SignalType::Approved),
    t("review", "no", SignalType::Rejected),
    t("review", "submit", SignalType::Returned),
  ];
  validate_definition(&snapshot.nodes, &snapshot.transitions).expect("validates");

  let events = vec![
    ev(1, event_type::STARTED, json!({ "context": null })),
    ev(
      2,
      event_type::ACTIVITY_SCHEDULED,
      json!({ "activity_id": A1, "activity_definition_id": "submit", "round": 1, "assignee_role": "nurse" }),
    ),
    ev(
      3,
      event_type::SIGNAL_RECEIVED,
      json!({ "signal_type": "approved", "target_activity_id": A1, "reviewer_id": "7" }),
    ),
    ev(4, event_type::ACTIVITY_COMPLETED, json!({ "activity_id": A1, "result": "approved" })),
    ev(
      5,
      event_type::ACTIVITY_SCHEDULED,
      json!({ "activity_id": A2, "activity_definition_id": "review", "round": 1, "assignee_role": "head_nurse" }),
    ),
    ev(
      6,
      event_type::SIGNAL_RECEIVED,
      json!({ "signal_type": "returned", "target_activity_id": A2, "reviewer_id": "9" }),
    ),
    ev(7, event_type::ACTIVITY_COMPLETED, json!({ "activity_id": A2, "result": "returned" })),
    ev(
      8,
      event_type::RETURNED_TO_REWORK,
      json!({
        "from_node": "review", "rework_target": "submit", "round": 2,
        "activity_id": A3, "assignee_role": "nurse"
      }),
    ),
  ];

  let projection = fold_events(&snapshot, &events);
  assert_eq!(projection.instance.status, WorkflowStatus::ReturnedWaiting);
  assert_eq!(projection.instance.current_activity_id.as_deref(), Some(A3));
  assert_eq!(projection.activities.len(), 3);
  let rework = &projection.activities[2];
  assert_eq!(rework.id, A3);
  assert_eq!(rework.activity_definition_id, "submit");
  assert_eq!(rework.status, ActivityStatus::Active);
  assert_eq!(rework.round, 2);
}

/// Parallel fan-out: `via` marks the branch scheduling so the instance keeps no single current
/// activity, and the join advances only once both branches arrived.
#[test]
fn parallel_fan_out_rebuilds_without_a_current_activity() {
  let snapshot = DefinitionSnapshot {
    flow_type: "test.parallel".into(),
    version: 1,
    max_round: 0,
    nodes: vec![
      start("start"),
      approval("triage", "head_nurse"),
      WorkflowNode {
        id: "fanout".into(),
        name: "fanout".into(),
        context_writes: Vec::new(),
        config: NodeConfig::ParallelSplit { branch_targets: vec!["left".into(), "right".into()] },
      },
      approval("left", "nurse"),
      approval("right", "physician"),
      WorkflowNode {
        id: "join".into(),
        name: "join".into(),
        context_writes: Vec::new(),
        config: NodeConfig::ParallelJoin { strategy: JoinStrategy::WaitAll },
      },
      end("ok", "approved"),
    ],
    transitions: vec![
      t("start", "triage", SignalType::Completed),
      t("triage", "fanout", SignalType::Approved),
      t("left", "join", SignalType::Approved),
      t("right", "join", SignalType::Approved),
      t("join", "ok", SignalType::Approved),
    ],
  };
  validate_definition(&snapshot.nodes, &snapshot.transitions).expect("validates");

  let events = vec![
    ev(1, event_type::STARTED, json!({})),
    ev(
      2,
      event_type::ACTIVITY_SCHEDULED,
      json!({ "activity_id": A1, "activity_definition_id": "triage", "round": 1, "assignee_role": "head_nurse" }),
    ),
    ev(
      3,
      event_type::SIGNAL_RECEIVED,
      json!({ "signal_type": "approved", "target_activity_id": A1, "reviewer_id": "1" }),
    ),
    ev(4, event_type::ACTIVITY_COMPLETED, json!({ "activity_id": A1, "result": "approved" })),
    ev(
      5,
      event_type::PARALLEL_SPLIT_SCHEDULED,
      json!({ "from": "triage", "branch_targets": ["left", "right"], "round": 1 }),
    ),
    ev(
      6,
      event_type::ACTIVITY_SCHEDULED,
      json!({
        "activity_id": A2, "activity_definition_id": "left", "round": 1,
        "assignee_role": "nurse", "via": "parallel_split"
      }),
    ),
    ev(
      7,
      event_type::ACTIVITY_SCHEDULED,
      json!({
        "activity_id": A3, "activity_definition_id": "right", "round": 1,
        "assignee_role": "physician", "via": "parallel_split"
      }),
    ),
  ];

  let projection = fold_events(&snapshot, &events);
  assert_eq!(projection.instance.status, WorkflowStatus::Active);
  assert_eq!(projection.instance.current_activity_id, None, "parallel fan-out has no single current activity");
  assert_eq!(projection.activities.len(), 3);
  assert!(projection.activities[1..].iter().all(|a| a.status == ActivityStatus::Active));
}

/// EventWait timeout: `activity.skipped.v1` with an explicit id skips exactly that activity.
#[test]
fn event_wait_timeout_skips_only_the_waiting_activity() {
  let snapshot = DefinitionSnapshot {
    flow_type: "test.event_wait".into(),
    version: 1,
    max_round: 0,
    nodes: vec![
      start("start"),
      approval("triage", "head_nurse"),
      WorkflowNode {
        id: "wait".into(),
        name: "wait".into(),
        context_writes: Vec::new(),
        config: NodeConfig::EventWait {
          event_type: "ext.ack".into(),
          timeout_seconds: Some(60),
          timeout_target: Some("ok".to_string()),
          correlation_key: None,
          source: None,
        },
      },
      end("ok", "approved"),
    ],
    transitions: vec![
      t("start", "triage", SignalType::Completed),
      t("triage", "wait", SignalType::Approved),
      t("wait", "ok", SignalType::EventReceived),
    ],
  };
  validate_definition(&snapshot.nodes, &snapshot.transitions).expect("validates");

  let events = vec![
    ev(1, event_type::STARTED, json!({})),
    ev(
      2,
      event_type::ACTIVITY_SCHEDULED,
      json!({ "activity_id": A1, "activity_definition_id": "triage", "round": 1, "assignee_role": "head_nurse" }),
    ),
    ev(
      3,
      event_type::SIGNAL_RECEIVED,
      json!({ "signal_type": "approved", "target_activity_id": A1, "reviewer_id": "1" }),
    ),
    ev(4, event_type::ACTIVITY_COMPLETED, json!({ "activity_id": A1, "result": "approved" })),
    ev(
      5,
      event_type::ACTIVITY_SCHEDULED,
      json!({ "activity_id": A2, "activity_definition_id": "wait", "round": 1, "assignee_role": null }),
    ),
    ev(6, event_type::TIMER_FIRED, json!({ "timer_kind": "timeout", "activity_id": A2 })),
    ev(7, event_type::ACTIVITY_SKIPPED, json!({ "activity_id": A2 })),
    ev(8, event_type::COMPLETED, json!({ "result": "approved" })),
  ];

  let projection = fold_events(&snapshot, &events);
  assert_eq!(projection.instance.status, WorkflowStatus::Completed);
  assert_eq!(projection.activities[0].status, ActivityStatus::Completed, "the approval stays completed");
  assert_eq!(projection.activities[1].status, ActivityStatus::Skipped);
  assert_eq!(projection.activities[1].activity_type, ActivityType::EventWait);
}

/// Notification delivery completes its activity without a preceding signal; the reducer falls
/// back to the system reviewer sentinel only when `source` says so.
#[test]
fn notification_outbox_completion_uses_the_system_sentinel() {
  let snapshot = DefinitionSnapshot {
    flow_type: "test.notify".into(),
    version: 1,
    max_round: 0,
    nodes: vec![
      start("start"),
      approval("triage", "head_nurse"),
      WorkflowNode {
        id: "notify".into(),
        name: "notify".into(),
        context_writes: Vec::new(),
        config: NodeConfig::Notification(hetuflow_core::NotificationConfig {
          template_code: Some("demo_template".into()),
          recipient_selector: Some(hetuflow_core::RecipientSelector::Role {
            role_code: "nurse".into(),
            facility_id: None,
          }),
          ..Default::default()
        }),
      },
      end("ok", "approved"),
    ],
    transitions: vec![
      t("start", "triage", SignalType::Completed),
      t("triage", "notify", SignalType::Approved),
      t("notify", "ok", SignalType::Completed),
    ],
  };
  validate_definition(&snapshot.nodes, &snapshot.transitions).expect("validates");

  let events = vec![
    ev(1, event_type::STARTED, json!({})),
    ev(
      2,
      event_type::ACTIVITY_SCHEDULED,
      json!({ "activity_id": A1, "activity_definition_id": "triage", "round": 1, "assignee_role": "head_nurse" }),
    ),
    ev(
      3,
      event_type::SIGNAL_RECEIVED,
      json!({ "signal_type": "approved", "target_activity_id": A1, "reviewer_id": "1" }),
    ),
    ev(4, event_type::ACTIVITY_COMPLETED, json!({ "activity_id": A1, "result": "approved" })),
    ev(
      5,
      event_type::ACTIVITY_SCHEDULED,
      json!({ "activity_id": A2, "activity_definition_id": "notify", "round": 1, "assignee_role": null }),
    ),
    ev(
      6,
      event_type::ACTIVITY_COMPLETED,
      json!({ "activity_id": A2, "result": null, "source": "notification_outbox" }),
    ),
    ev(7, event_type::COMPLETED, json!({ "result": "approved" })),
  ];

  let projection = fold_events(&snapshot, &events);
  let notify = &projection.activities[1];
  assert_eq!(notify.activity_type, ActivityType::Notification);
  assert_eq!(notify.status, ActivityStatus::Completed);
  assert_eq!(notify.result, None);
  assert_eq!(notify.reviewed_by.as_deref(), Some("0"), "system sentinel, not a human reviewer");
}
