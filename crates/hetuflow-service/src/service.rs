//! Transactional orchestration: the single place where a runtime decision becomes durable state.
//!
//! Every public method here runs **inside the caller's transaction** (`&DbxPostgres`), exactly like
//! `hetuflow-sqlx` (A1 invariant). The engine never opens a transaction and never sets a session
//! variable — so "append the fact, update the projection, create the timer/outbox intent" is atomic
//! with whatever else the adapter's handler is doing.
//!
//! ## Event payload contract
//!
//! `hetuflow_runtime::replay::fold_events` rebuilds the canonical projection from the event log, so
//! the payloads written here are a **contract**, not free-form logging. Changing a payload key
//! without changing the reducer breaks rebuild. The keys each event carries are documented at its
//! `append_*` call site.

use std::collections::BTreeMap;

use fusion_sql::store::DbxPostgres;
use hetuflow_core::{
  ActivityInstance, ActivityType, AdvanceOutcome, BusinessCallbackPayload, DefinitionSnapshot, FlowError,
  IdempotencyKeyStrategy, JoinStrategy, NodeKind, NotificationConfig, NotificationPayload, ParallelTarget,
  RecipientSelector, Result, ScopeFilter, SignalType, WorkflowInstance, WorkflowNode, WorkflowResult, WorkflowStatus,
  event_type, outbox_activity,
};
use hetuflow_runtime::replay::{diff_projection, fold_events, instantiated_activity_type};
use hetuflow_runtime::{
  ResolveTrace, decide_advance, decide_event_wait_timeout, decide_start, is_join_incoming_signal, validate_definition,
};
use hetuflow_sqlx::{PgWorkflowStore, WorkflowStore};
use serde_json::{Value, json};

use crate::command::{
  ProjectionVerification, SignalCommand, SignalOutcome, StartCommand, StartOutcome, TerminalCallback,
};
use crate::ports::CallbackRegistry;

const STORE: PgWorkflowStore = PgWorkflowStore::new();

/// `workflow.started.v1` payload key carrying the instance-level terminal callback intent.
/// The event log is the fact source, so the intent lives there rather than polluting business
/// `context` (which the `context_writes` whitelist may legitimately rewrite).
const STARTED_TERMINAL_CALLBACK: &str = "terminal_callback";

/// Timer kinds. `reminder` / `escalation` come from the store's Phase F2 contract; `timeout` is
/// this crate's third kind for EventWait deadlines (the column is an opaque string to the store).
pub mod timer_kind {
  pub const REMINDER: &str = "reminder";
  pub const ESCALATION: &str = "escalation";
  pub const TIMEOUT: &str = "timeout";
}

/// Framework-level ceiling on rework rounds. The effective limit is
/// `min(definition.max_round, this)` when the definition declares one, else this.
pub const MAX_ROUND_HARD_LIMIT: i32 = 20;

/// Adapter-supplied orchestration policy. Everything here is application vocabulary the framework
/// only forwards — notification template codes and channel policy are never invented by the engine.
#[derive(Debug, Clone)]
pub struct WorkflowConfig {
  /// Template code for the SLA reminder notification on an approval activity.
  pub reminder_template_code: String,
  /// Template code for the escalation notification after an approval is escalated.
  pub escalation_template_code: String,
  /// Channel policy applied to the two framework-generated notifications above.
  pub notification_types: Vec<String>,
  /// `reference_type` stamped on framework-generated notification payloads.
  pub notification_reference_type: String,
  /// `purpose` stamped on framework-generated notification payloads.
  pub notification_purpose: String,
  /// `visibility` stamped on framework-generated notification payloads.
  pub notification_visibility: String,
  /// Ceiling on rework rounds; clamped to [`MAX_ROUND_HARD_LIMIT`].
  pub max_round_hard_limit: i32,
}

impl Default for WorkflowConfig {
  fn default() -> Self {
    Self {
      reminder_template_code: "workflow_approval_reminder".to_string(),
      escalation_template_code: "workflow_approval_escalation".to_string(),
      notification_types: vec!["NOTIFICATION_TYPE_IN_APP".to_string()],
      notification_reference_type: "workflow_instance".to_string(),
      notification_purpose: "workflow_notification".to_string(),
      notification_visibility: "VISIBILITY_ACTION".to_string(),
      max_round_hard_limit: MAX_ROUND_HARD_LIMIT,
    }
  }
}

/// The orchestration service.
///
/// Concrete over [`PgWorkflowStore`] on purpose: `WorkflowStore`'s `async fn`s carry no `Send`
/// bound (its doc comment pins that to the single in-tree impl), so a generic service would make
/// every future's `Send`-ness unprovable and `tokio::spawn` of the workers impossible.
pub struct WorkflowService {
  config: WorkflowConfig,
  registry: CallbackRegistry,
}

impl WorkflowService {
  pub fn new(config: WorkflowConfig, registry: CallbackRegistry) -> Self {
    let mut config = config;
    config.max_round_hard_limit = config.max_round_hard_limit.clamp(1, MAX_ROUND_HARD_LIMIT);
    Self { config, registry }
  }

  pub fn config(&self) -> &WorkflowConfig {
    &self.config
  }

  pub fn registry(&self) -> &CallbackRegistry {
    &self.registry
  }

  // ===========================================================================
  // start
  // ===========================================================================

  /// Start an instance. Idempotent per `(business_type, business_key)`: an existing non-terminal
  /// instance is returned unchanged (`created = false`).
  pub async fn start(&self, dbx: &DbxPostgres, cmd: StartCommand) -> Result<StartOutcome> {
    // Validate before writing anything (framework spec §5) — a definition that passed a publish
    // gate months ago is still re-validated here; the engine never trusts "it was checked once".
    validate_definition(&cmd.snapshot.nodes, &cmd.snapshot.transitions)?;

    // The start entry is the definition's first APPROVAL node (`decide_start`). A definition
    // without one has no human entry point and cannot be executed.
    let Some((entry_node_id, entry_role)) = decide_start(&cmd.snapshot.nodes) else {
      return Err(FlowError::Validation(
        "definition has no approval node to enter (decide_start found no entry activity)".into(),
      ));
    };

    if let Some(tc) = &cmd.terminal_callback {
      if tc.handler_type.is_empty() {
        return Err(FlowError::Validation("terminal_callback.handler_type must not be empty".into()));
      }
      // Fail-closed at start: reaching a terminal state only to find nobody can run the side
      // effect would leave the business object silently half-done.
      if !self.registry.contains(&tc.handler_type) {
        return Err(FlowError::Validation(format!(
          "terminal callback handler '{}' is not registered (known: {:?})",
          tc.handler_type,
          self.registry.handler_types()
        )));
      }
    }

    let snapshot_json = serde_json::to_value(&cmd.snapshot)
      .map_err(|e| FlowError::Serialization(format!("definition snapshot not serializable: {e}")))?;
    let hash = hetuflow_core::definition_hash(&snapshot_json);

    let inserted = STORE
      .insert_instance_on_conflict(
        dbx,
        cmd.tenant_id,
        cmd.facility_id,
        cmd.definition_id,
        &cmd.reference_no,
        &cmd.business_key,
        &cmd.business_type,
        cmd.context.as_ref(),
        &snapshot_json,
        &hash,
      )
      .await?;

    let Some(instance) = inserted else {
      // Idempotent hit: an active instance already exists for this business object.
      let existing = STORE
        .active_instance_by_business_key(dbx, &cmd.business_key, &cmd.business_type)
        .await?
        .ok_or_else(|| {
          FlowError::Conflict(format!(
            "start conflicted on ({}, {}) but no active instance is readable",
            cmd.business_type, cmd.business_key
          ))
        })?;
      let instance_id = parse_uuid(&existing.id)?;
      let active = STORE.active_activities(dbx, instance_id).await?;
      return Ok(StartOutcome { instance: existing, created: false, active_activities: active });
    };

    let instance_id = parse_uuid(&instance.id)?;
    let mut ctx = InstanceCtx {
      instance_id,
      tenant_id: cmd.tenant_id,
      facility_id: cmd.facility_id,
      snapshot: cmd.snapshot,
      context: cmd.context.clone(),
      business_type: cmd.business_type.clone(),
      business_key: cmd.business_key.clone(),
      reference_no: cmd.reference_no.clone(),
    };

    // `workflow.started.v1` — replay reads only the timestamp, but the payload also carries the
    // terminal-callback intent (read back at completion) and the initial context for auditability.
    self
      .append(
        dbx,
        &ctx,
        event_type::STARTED,
        json!({
          "flow_type": ctx.snapshot.flow_type,
          "version": ctx.snapshot.version,
          "definition_hash": hash,
          "business_type": ctx.business_type,
          "business_key": ctx.business_key,
          "context": ctx.context.clone().unwrap_or(Value::Null),
          STARTED_TERMINAL_CALLBACK: match &cmd.terminal_callback {
            Some(tc) => serde_json::to_value(tc).unwrap_or(Value::Null),
            None => Value::Null,
          },
        }),
        None,
      )
      .await?;

    let entry_node = ctx.node(&entry_node_id)?.clone();
    let activity = self.schedule_activity(dbx, &mut ctx, &entry_node, entry_role.as_deref(), 1, None).await?;
    STORE.update_current_activity(dbx, instance_id, parse_uuid(&activity.id)?).await?;

    let instance = self.reload(dbx, instance_id).await?;
    let active = STORE.active_activities(dbx, instance_id).await?;
    Ok(StartOutcome { instance, created: true, active_activities: active })
  }

  // ===========================================================================
  // signal
  // ===========================================================================

  /// Advance an instance with one signal, inside the caller's transaction.
  pub async fn signal(&self, dbx: &DbxPostgres, cmd: SignalCommand) -> Result<SignalOutcome> {
    if cmd.idempotency_key.trim().is_empty() {
      return Err(FlowError::Validation("idempotency_key is required".into()));
    }
    // CQ1 single-writer: the row lock serializes concurrent signals on one instance.
    let instance = STORE
      .load_instance_for_update_scoped(dbx, &cmd.scope, cmd.instance_id)
      .await?
      .ok_or_else(|| FlowError::NotFound(format!("workflow instance {} not visible", cmd.instance_id)))?;

    if STORE.idempotency_seen(dbx, cmd.instance_id, &cmd.idempotency_key).await? {
      let active = STORE.active_activities(dbx, cmd.instance_id).await?;
      return Ok(SignalOutcome { instance, active_activities: active, replayed: true });
    }
    if instance.status.is_terminal() {
      return Err(FlowError::Conflict(format!(
        "workflow instance {} is already terminal ({})",
        cmd.instance_id,
        instance.status.as_str()
      )));
    }

    let snapshot = STORE.load_snapshot(dbx, cmd.instance_id).await?;
    let mut ctx = InstanceCtx {
      instance_id: cmd.instance_id,
      tenant_id: parse_i64(&instance.tenant_id, "tenant_id")?,
      facility_id: parse_uuid(&instance.facility_id)?,
      snapshot,
      context: instance.context.clone(),
      business_type: instance.business_type.clone(),
      business_key: instance.business_key.clone(),
      reference_no: instance.reference_no.clone(),
    };

    let activity = self.resolve_target_activity(dbx, &cmd).await?;
    let activity_id = parse_uuid(&activity.id)?;
    let node = ctx.node(&activity.activity_definition_id)?.clone();
    check_signal_against_node(&node, cmd.signal, cmd.event_type.as_deref())?;

    // --- claim / reassign: assignee bookkeeping only, the graph does not move ---
    if matches!(cmd.signal, SignalType::Claimed | SignalType::Reassigned) {
      STORE.update_activity_assignee(dbx, activity_id, cmd.assignee_id).await?;
      self
        .append(
          dbx,
          &ctx,
          event_type::SIGNAL_RECEIVED,
          json!({
            "signal_type": signal_str(cmd.signal),
            "target_activity_id": activity.id,
            "assignee_id": cmd.assignee_id.map(|v| v.to_string()),
          }),
          Some(&cmd.idempotency_key),
        )
        .await?;
      let instance = self.reload(dbx, cmd.instance_id).await?;
      let active = STORE.active_activities(dbx, cmd.instance_id).await?;
      return Ok(SignalOutcome { instance, active_activities: active, replayed: false });
    }

    // --- runtime context write-back (framework gaps §8), before the advance decision ---
    self.apply_context_writes(dbx, &mut ctx, &node, activity.round, cmd.context_patch.as_ref()).await?;

    self
      .append(
        dbx,
        &ctx,
        event_type::SIGNAL_RECEIVED,
        json!({
          "signal_type": signal_str(cmd.signal),
          "target_activity_id": activity.id,
          "reviewer_id": cmd.actor_user_id.to_string(),
          "review_notes": cmd.review_notes.clone(),
        }),
        Some(&cmd.idempotency_key),
      )
      .await?;

    self
      .complete_activity(
        dbx,
        &ctx,
        &activity,
        signal_result(cmd.signal),
        cmd.actor_user_id,
        cmd.review_notes.as_deref(),
      )
      .await?;

    // Precise rework resume: the target activity of a `returned_waiting` instance just cleared, so
    // the instance re-enters the main chain before any CAS that requires `status = 'active'`.
    if instance.status == WorkflowStatus::ReturnedWaiting
      && STORE.resume_from_returned_waiting(dbx, cmd.instance_id).await?
    {
      self
        .append(
          dbx,
          &ctx,
          event_type::REWORK_RESUMED,
          json!({ "rework_target": activity.activity_definition_id, "round": activity.round }),
          None,
        )
        .await?;
    }

    let (outcome, trace) = decide_advance(
      &ctx.snapshot.nodes,
      &ctx.snapshot.transitions,
      &activity.activity_definition_id,
      cmd.signal,
      ctx.context.as_ref(),
    );
    self.apply_outcome(dbx, &mut ctx, outcome, &trace, &activity, activity.round).await?;

    let instance = self.reload(dbx, cmd.instance_id).await?;
    let active = STORE.active_activities(dbx, cmd.instance_id).await?;
    Ok(SignalOutcome { instance, active_activities: active, replayed: false })
  }

  /// Whole-workflow resubmit: a `completed(returned)` instance re-enters from the start node.
  /// Precise rework does NOT come here — it is handled inside [`Self::signal`].
  pub async fn resubmit(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    instance_id: uuid::Uuid,
    idempotency_key: &str,
  ) -> Result<SignalOutcome> {
    if idempotency_key.trim().is_empty() {
      return Err(FlowError::Validation("idempotency_key is required".into()));
    }
    let instance = STORE
      .load_instance_for_update_scoped(dbx, scope, instance_id)
      .await?
      .ok_or_else(|| FlowError::NotFound(format!("workflow instance {instance_id} not visible")))?;
    if STORE.idempotency_seen(dbx, instance_id, idempotency_key).await? {
      let active = STORE.active_activities(dbx, instance_id).await?;
      return Ok(SignalOutcome { instance, active_activities: active, replayed: true });
    }
    if !(instance.status == WorkflowStatus::Completed && instance.result == Some(WorkflowResult::Returned)) {
      return Err(FlowError::InvalidState(format!(
        "resubmit requires a completed(returned) instance, got {} ({:?})",
        instance.status.as_str(),
        instance.result
      )));
    }

    let snapshot = STORE.load_snapshot(dbx, instance_id).await?;
    let mut ctx = InstanceCtx {
      instance_id,
      tenant_id: parse_i64(&instance.tenant_id, "tenant_id")?,
      facility_id: parse_uuid(&instance.facility_id)?,
      snapshot,
      context: instance.context.clone(),
      business_type: instance.business_type.clone(),
      business_key: instance.business_key.clone(),
      reference_no: instance.reference_no.clone(),
    };

    let round = STORE.max_round(dbx, instance_id).await? + 1;
    self.check_round(&ctx, round)?;
    if !STORE.reactivate_workflow(dbx, instance_id).await? {
      return Err(FlowError::Conflict("reactivate lost the race (instance no longer completed/returned)".into()));
    }
    self
      .append(dbx, &ctx, event_type::REACTIVATED, json!({ "round": round }), Some(idempotency_key))
      .await?;

    let Some((entry_node_id, entry_role)) = decide_start(&ctx.snapshot.nodes) else {
      return Err(FlowError::Validation("definition has no approval node to re-enter".into()));
    };
    let entry_node = ctx.node(&entry_node_id)?.clone();
    let activity = self.schedule_activity(dbx, &mut ctx, &entry_node, entry_role.as_deref(), round, None).await?;
    STORE.update_current_activity(dbx, instance_id, parse_uuid(&activity.id)?).await?;

    let instance = self.reload(dbx, instance_id).await?;
    let active = STORE.active_activities(dbx, instance_id).await?;
    Ok(SignalOutcome { instance, active_activities: active, replayed: false })
  }

  // ===========================================================================
  // timers
  // ===========================================================================

  /// Fire one due timer inside the caller's transaction. Idempotent: a timer already fired or
  /// cancelled, or an activity that has since left `active`, is a no-op.
  pub async fn fire_timer(&self, dbx: &DbxPostgres, timer: &hetuflow_core::TimerRecord) -> Result<bool> {
    let timer_id = parse_uuid(&timer.id)?;
    if !STORE.mark_timer_fired(dbx, timer_id).await? {
      return Ok(false); // lost the race / already handled
    }
    let instance_id = parse_uuid(&timer.workflow_instance_id)?;
    let activity_id = parse_uuid(&timer.activity_instance_id)?;
    let Some(instance) = STORE.load_instance(dbx, instance_id).await? else { return Ok(false) };
    if instance.status.is_terminal() {
      return Ok(false);
    }
    let Some(activity) = STORE.load_activity(dbx, activity_id).await? else { return Ok(false) };
    if activity.status != hetuflow_core::ActivityStatus::Active {
      return Ok(false);
    }

    let snapshot = STORE.load_snapshot(dbx, instance_id).await?;
    let mut ctx = InstanceCtx {
      instance_id,
      tenant_id: parse_i64(&instance.tenant_id, "tenant_id")?,
      facility_id: parse_uuid(&instance.facility_id)?,
      snapshot,
      context: instance.context.clone(),
      business_type: instance.business_type.clone(),
      business_key: instance.business_key.clone(),
      reference_no: instance.reference_no.clone(),
    };
    let node = ctx.node(&activity.activity_definition_id)?.clone();

    self
      .append(
        dbx,
        &ctx,
        event_type::TIMER_FIRED,
        json!({ "timer_kind": timer.timer_kind, "activity_id": activity.id, "activity_definition_id": node.id }),
        None,
      )
      .await?;

    match timer.timer_kind.as_str() {
      timer_kind::REMINDER => {
        let payload = self.notification_payload(
          &ctx,
          self.config.reminder_template_code.clone(),
          node.assignee_role().or(activity.assignee_role.as_deref()),
          &node,
        );
        self
          .enqueue_notification(dbx, &ctx, &activity, outbox_activity::NOTIFICATION_REMINDER, &payload)
          .await?;
      }
      timer_kind::ESCALATION => {
        let Some((_, to_role)) = node.escalation_config() else {
          return Ok(false); // escalation config removed by a later definition version — nothing to do
        };
        if STORE.update_activity_assignee_role(dbx, activity_id, to_role).await? {
          self
            .append(
              dbx,
              &ctx,
              event_type::APPROVAL_ESCALATED,
              json!({
                "activity_id": activity.id,
                "from_role": activity.assignee_role.clone(),
                "to_role": to_role,
              }),
              None,
            )
            .await?;
          let payload =
            self.notification_payload(&ctx, self.config.escalation_template_code.clone(), Some(to_role), &node);
          self
            .enqueue_notification(dbx, &ctx, &activity, outbox_activity::NOTIFICATION_ESCALATION, &payload)
            .await?;
        }
      }
      timer_kind::TIMEOUT => {
        STORE.skip_activity_for_timeout(dbx, activity_id).await?;
        STORE.cancel_timers_for_activity(dbx, activity_id).await?;
        self
          .append(dbx, &ctx, event_type::ACTIVITY_SKIPPED, json!({ "activity_id": activity.id }), None)
          .await?;
        let (outcome, trace) = decide_event_wait_timeout(
          &ctx.snapshot.nodes,
          &ctx.snapshot.transitions,
          &activity.activity_definition_id,
          ctx.context.as_ref(),
        );
        self.apply_outcome(dbx, &mut ctx, outcome, &trace, &activity, activity.round).await?;
      }
      other => {
        return Err(FlowError::InvalidState(format!("unknown timer_kind '{other}'")));
      }
    }
    Ok(true)
  }

  // ===========================================================================
  // outbox delivery follow-up (called by the dispatcher after the port confirms)
  // ===========================================================================

  /// Notification delivered: complete the notification activity and advance. Called in the
  /// record's tenant transaction after the dispatcher port reported success.
  pub async fn on_notification_delivered(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
  ) -> Result<()> {
    let Some(instance) = STORE.load_instance(dbx, instance_id).await? else { return Ok(()) };
    if instance.status.is_terminal() {
      return Ok(());
    }
    let Some(activity) = STORE.load_activity(dbx, activity_id).await? else { return Ok(()) };
    if activity.status != hetuflow_core::ActivityStatus::Active {
      return Ok(()); // reminder / escalation notifications ride an already-active approval
    }
    if activity.activity_type != ActivityType::Notification {
      return Ok(()); // reminder / escalation: delivery does not complete the approval
    }

    let snapshot = STORE.load_snapshot(dbx, instance_id).await?;
    let mut ctx = InstanceCtx {
      instance_id,
      tenant_id: parse_i64(&instance.tenant_id, "tenant_id")?,
      facility_id: parse_uuid(&instance.facility_id)?,
      snapshot,
      context: instance.context.clone(),
      business_type: instance.business_type.clone(),
      business_key: instance.business_key.clone(),
      reference_no: instance.reference_no.clone(),
    };

    STORE.complete_activity_cas(dbx, activity_id, None, 0, None).await?;
    STORE.cancel_timers_for_activity(dbx, activity_id).await?;
    // `source: notification_outbox` tells the reducer there was no preceding SIGNAL_RECEIVED, so
    // reviewer bookkeeping falls back to the system sentinel.
    self
      .append(
        dbx,
        &ctx,
        event_type::ACTIVITY_COMPLETED,
        json!({ "activity_id": activity.id, "result": Value::Null, "source": "notification_outbox" }),
        None,
      )
      .await?;

    let (outcome, trace) = decide_advance(
      &ctx.snapshot.nodes,
      &ctx.snapshot.transitions,
      &activity.activity_definition_id,
      SignalType::Completed,
      ctx.context.as_ref(),
    );
    self.apply_outcome(dbx, &mut ctx, outcome, &trace, &activity, activity.round).await
  }

  /// Terminal business callback delivered: flip `side_effects_executed`.
  pub async fn on_business_callback_delivered(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<()> {
    STORE.mark_side_effects_executed(dbx, instance_id).await?;
    Ok(())
  }

  /// Outbox delivery gave up (dead letter): record the failure fact on the event log so the
  /// timeline shows *why* a workflow's side effect never happened.
  pub async fn on_outbox_dead_letter(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
    activity_type: &str,
    last_error: &str,
  ) -> Result<()> {
    let Some(instance) = STORE.load_instance(dbx, instance_id).await? else { return Ok(()) };
    let ctx = InstanceCtx {
      instance_id,
      tenant_id: parse_i64(&instance.tenant_id, "tenant_id")?,
      facility_id: parse_uuid(&instance.facility_id)?,
      snapshot: DefinitionSnapshot {
        flow_type: String::new(),
        version: 0,
        nodes: Vec::new(),
        transitions: Vec::new(),
        max_round: 0,
      },
      context: None,
      business_type: instance.business_type.clone(),
      business_key: instance.business_key.clone(),
      reference_no: instance.reference_no.clone(),
    };
    self
      .append(
        dbx,
        &ctx,
        event_type::ACTIVITY_FAILED,
        json!({ "activity_id": activity_id.to_string(), "outbox_activity_type": activity_type, "error": last_error }),
        None,
      )
      .await
  }

  // ===========================================================================
  // read-only: projection drift
  // ===========================================================================

  /// Rebuild the canonical projection from the bound snapshot + event log and diff it against the
  /// live rows. Read-only by contract (framework projection-traceability §4).
  pub async fn verify_projection(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    instance_id: uuid::Uuid,
  ) -> Result<ProjectionVerification> {
    let instance = STORE
      .get_instance_scoped(dbx, scope, instance_id)
      .await?
      .ok_or_else(|| FlowError::NotFound(format!("workflow instance {instance_id} not visible")))?;
    let snapshot = STORE.load_snapshot(dbx, instance_id).await?;
    let events = STORE.load_events(dbx, instance_id).await?;
    let activities = STORE.list_activities(dbx, instance_id).await?;
    let replayed = fold_events(&snapshot, &events);
    let differences = diff_projection(&replayed, &instance, &activities);
    Ok(ProjectionVerification {
      instance_id,
      event_count: events.len() as i64,
      consistent: differences.is_empty(),
      differences,
    })
  }

  // ===========================================================================
  // internals
  // ===========================================================================

  async fn reload(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<WorkflowInstance> {
    STORE
      .load_instance(dbx, instance_id)
      .await?
      .ok_or_else(|| FlowError::NotFound(format!("workflow instance {instance_id} vanished mid-transaction")))
  }

  async fn resolve_target_activity(&self, dbx: &DbxPostgres, cmd: &SignalCommand) -> Result<ActivityInstance> {
    match cmd.activity_id {
      Some(id) => {
        let activity = STORE
          .load_activity(dbx, id)
          .await?
          .ok_or_else(|| FlowError::NotFound(format!("activity {id} not found")))?;
        if activity.workflow_instance_id != cmd.instance_id.to_string() {
          return Err(FlowError::Validation(format!("activity {id} does not belong to instance {}", cmd.instance_id)));
        }
        if activity.status != hetuflow_core::ActivityStatus::Active {
          return Err(FlowError::Conflict(format!("activity {id} is {}, not active", activity.status.as_str())));
        }
        Ok(activity)
      }
      None => {
        let mut active = STORE.active_activities(dbx, cmd.instance_id).await?;
        match active.len() {
          0 => Err(FlowError::Conflict(format!("workflow instance {} has no active activity", cmd.instance_id))),
          1 => Ok(active.remove(0)),
          n => Err(FlowError::Validation(format!(
            "workflow instance {} has {n} active activities — activity_id is required",
            cmd.instance_id
          ))),
        }
      }
    }
  }

  /// Merge the declared `context_writes` keys, fail-closed on a declared-but-absent key.
  async fn apply_context_writes(
    &self,
    dbx: &DbxPostgres,
    ctx: &mut InstanceCtx,
    node: &WorkflowNode,
    round: i32,
    patch: Option<&Value>,
  ) -> Result<()> {
    let declared = node.context_writes();
    if declared.is_empty() {
      return Ok(());
    }
    let obj = patch.and_then(Value::as_object).ok_or_else(|| {
      FlowError::Validation(format!(
        "node '{}' declares context_writes {declared:?} but the signal carried no context patch object",
        node.id
      ))
    })?;
    let mut filtered = serde_json::Map::new();
    for key in declared {
      let value = obj.get(key).ok_or_else(|| {
        FlowError::Validation(format!("node '{}' context_writes key '{key}' missing from the context patch", node.id))
      })?;
      filtered.insert(key.clone(), value.clone());
    }
    let merged = STORE.merge_instance_context(dbx, ctx.instance_id, &Value::Object(filtered)).await?;
    ctx.context = Some(merged);
    self
      .append(dbx, ctx, event_type::CONTEXT_UPDATED, json!({ "node": node.id, "keys": declared, "round": round }), None)
      .await
  }

  async fn complete_activity(
    &self,
    dbx: &DbxPostgres,
    ctx: &InstanceCtx,
    activity: &ActivityInstance,
    result: Option<&str>,
    actor_user_id: i64,
    review_notes: Option<&str>,
  ) -> Result<()> {
    let activity_id = parse_uuid(&activity.id)?;
    if !STORE.complete_activity_cas(dbx, activity_id, result, actor_user_id, review_notes).await? {
      return Err(FlowError::Conflict(format!("activity {} is no longer active", activity.id)));
    }
    STORE.cancel_timers_for_activity(dbx, activity_id).await?;
    self
      .append(dbx, ctx, event_type::ACTIVITY_COMPLETED, json!({ "activity_id": activity.id, "result": result }), None)
      .await
  }

  /// Insert one activity row + its `activity.scheduled.v1` fact + its timers + (for Notification
  /// nodes) its delivery intent. `via` marks fan-out scheduling so the reducer knows the instance
  /// has no single current activity.
  async fn schedule_activity(
    &self,
    dbx: &DbxPostgres,
    ctx: &mut InstanceCtx,
    node: &WorkflowNode,
    assignee_role: Option<&str>,
    round: i32,
    fanout: Option<FanoutMark<'_>>,
  ) -> Result<ActivityInstance> {
    if node.kind() == NodeKind::SubWorkflow {
      // Executing a SubWorkflow node needs a "resolve definition by flow_type" port that the
      // store contract does not provide; scheduling one would strand the parent forever. Refuse
      // loudly instead (this node kind is out of the current delivery scope).
      return Err(FlowError::Validation(format!(
        "node '{}' is a sub_workflow node — child-instance orchestration is not wired in this build",
        node.id
      )));
    }
    let activity_type = instantiated_activity_type(node.kind());
    let activity = STORE
      .insert_activity(dbx, ctx.tenant_id, ctx.instance_id, &node.id, activity_type, assignee_role, round)
      .await?;
    let activity_id = parse_uuid(&activity.id)?;

    let mut payload = json!({
      "activity_id": activity.id,
      "activity_definition_id": node.id,
      "activity_type": activity_type.as_str(),
      "round": round,
      "assignee_role": assignee_role,
    });
    if let Some(mark) = &fanout
      && let Some(map) = payload.as_object_mut()
    {
      map.insert("via".to_string(), Value::String(mark.via.to_string()));
      if let Some(index) = mark.item_index {
        map.insert("item_index".to_string(), json!(index));
      }
      if let Some(item) = mark.item_payload {
        map.insert("item_payload".to_string(), item.clone());
      }
    }
    self.append(dbx, ctx, event_type::ACTIVITY_SCHEDULED, payload, None).await?;

    // Durable SLA / escalation timers (never an in-process sleep — framework spec §2).
    if let Some(sla) = node.sla_seconds() {
      STORE
        .schedule_timer(dbx, ctx.tenant_id, ctx.facility_id, ctx.instance_id, activity_id, timer_kind::REMINDER, sla)
        .await?;
    }
    if let Some((escalation_secs, _)) = node.escalation_config() {
      STORE
        .schedule_timer(
          dbx,
          ctx.tenant_id,
          ctx.facility_id,
          ctx.instance_id,
          activity_id,
          timer_kind::ESCALATION,
          escalation_secs,
        )
        .await?;
    }
    if let Some((_, Some(timeout_secs), _)) = node.event_wait() {
      STORE
        .schedule_timer(
          dbx,
          ctx.tenant_id,
          ctx.facility_id,
          ctx.instance_id,
          activity_id,
          timer_kind::TIMEOUT,
          timeout_secs,
        )
        .await?;
    }
    if let Some(config) = node.notification() {
      let payload = self.notification_from_config(ctx, config);
      self.enqueue_notification(dbx, ctx, &activity, outbox_activity::NOTIFICATION, &payload).await?;
    }
    Ok(activity)
  }

  async fn enqueue_notification(
    &self,
    dbx: &DbxPostgres,
    ctx: &InstanceCtx,
    activity: &ActivityInstance,
    outbox_type: &str,
    payload: &NotificationPayload,
  ) -> Result<()> {
    let value = serde_json::to_value(payload)
      .map_err(|e| FlowError::Serialization(format!("notification payload not serializable: {e}")))?;
    STORE
      .enqueue_outbox(
        dbx,
        ctx.tenant_id,
        ctx.facility_id,
        ctx.instance_id,
        parse_uuid(&activity.id)?,
        outbox_type,
        &value,
      )
      .await
  }

  fn notification_from_config(&self, ctx: &InstanceCtx, config: &NotificationConfig) -> NotificationPayload {
    NotificationPayload {
      tenant_id: ctx.tenant_id.to_string(),
      facility_id: Some(ctx.facility_id.to_string()),
      template_code: config.template_code.clone().unwrap_or_default(),
      recipient_selector: config
        .recipient_selector
        .clone()
        .unwrap_or_else(|| RecipientSelector::UserIds { user_ids: Vec::new() }),
      channel_policy: config.channel_policy.clone(),
      template_args: config.template_args.clone(),
      reference_type: config.reference_type.clone().unwrap_or_else(|| self.config.notification_reference_type.clone()),
      purpose: config.purpose.clone().unwrap_or_else(|| self.config.notification_purpose.clone()),
      visibility: config.visibility.clone().unwrap_or_else(|| self.config.notification_visibility.clone()),
      business_type: ctx.business_type.clone(),
      business_key: ctx.business_key.clone(),
    }
  }

  /// Framework-generated reminder / escalation notification. Template codes and channel policy
  /// come from the adapter's [`WorkflowConfig`] — the engine invents no routing.
  fn notification_payload(
    &self,
    ctx: &InstanceCtx,
    template_code: String,
    role_code: Option<&str>,
    node: &WorkflowNode,
  ) -> NotificationPayload {
    let mut template_args = BTreeMap::new();
    template_args.insert("reference_no".to_string(), ctx.reference_no.clone());
    template_args.insert("node_id".to_string(), node.id.clone());
    template_args.insert("node_name".to_string(), node.name.clone());
    NotificationPayload {
      tenant_id: ctx.tenant_id.to_string(),
      facility_id: Some(ctx.facility_id.to_string()),
      template_code,
      recipient_selector: match role_code {
        Some(role) => {
          RecipientSelector::Role { role_code: role.to_string(), facility_id: Some(ctx.facility_id.to_string()) }
        }
        None => RecipientSelector::UserIds { user_ids: Vec::new() },
      },
      channel_policy: hetuflow_core::NotificationChannelPolicy {
        notification_types: self.config.notification_types.clone(),
      },
      template_args,
      reference_type: self.config.notification_reference_type.clone(),
      purpose: self.config.notification_purpose.clone(),
      visibility: self.config.notification_visibility.clone(),
      business_type: ctx.business_type.clone(),
      business_key: ctx.business_key.clone(),
    }
  }

  fn check_round(&self, ctx: &InstanceCtx, round: i32) -> Result<()> {
    let declared = if ctx.snapshot.max_round > 0 { ctx.snapshot.max_round } else { self.config.max_round_hard_limit };
    let effective = declared.min(self.config.max_round_hard_limit);
    if round > effective {
      return Err(FlowError::Conflict(format!("rework round {round} exceeds the effective limit {effective}")));
    }
    Ok(())
  }

  async fn append(
    &self,
    dbx: &DbxPostgres,
    ctx: &InstanceCtx,
    event: &str,
    payload: Value,
    idempotency_key: Option<&str>,
  ) -> Result<()> {
    let seq = STORE.next_seq(dbx, ctx.instance_id).await?;
    STORE
      .append_event(dbx, ctx.tenant_id, ctx.facility_id, ctx.instance_id, seq, event, payload, idempotency_key)
      .await
  }

  // --- outcome application ------------------------------------------------

  #[allow(clippy::only_used_in_recursion)]
  async fn apply_outcome(
    &self,
    dbx: &DbxPostgres,
    ctx: &mut InstanceCtx,
    outcome: AdvanceOutcome,
    trace: &ResolveTrace,
    source_activity: &ActivityInstance,
    round: i32,
  ) -> Result<()> {
    // Serial Merge nodes are pure routing: they leave a fact, not a row.
    for merge_node in &trace.merges_passed {
      self
        .append(
          dbx,
          ctx,
          event_type::MERGE_PASSED,
          json!({ "from": source_activity.activity_definition_id, "merge_node": merge_node, "round": round }),
          None,
        )
        .await?;
    }

    match outcome {
      AdvanceOutcome::Advance { target_node, assignee_role } => {
        let node = ctx.node(&target_node)?.clone();
        if node.kind() == NodeKind::ParallelJoin {
          return Box::pin(self.branch_reached_join(dbx, ctx, &node, source_activity, round)).await;
        }
        let activity = self.schedule_activity(dbx, ctx, &node, assignee_role.as_deref(), round, None).await?;
        STORE.update_current_activity(dbx, ctx.instance_id, parse_uuid(&activity.id)?).await?;
        Ok(())
      }
      AdvanceOutcome::AdvanceMulti { targets } => {
        Box::pin(self.fan_out(dbx, ctx, targets, trace, source_activity, round)).await
      }
      AdvanceOutcome::Complete { result } => self.complete_instance(dbx, ctx, result).await,
      AdvanceOutcome::Rework { target_node, assignee_role } => {
        let next_round = STORE.max_round(dbx, ctx.instance_id).await? + 1;
        if let Err(e) = self.check_round(ctx, next_round) {
          self.fail_instance(dbx, ctx, &e.to_string()).await?;
          return Err(e);
        }
        let node = ctx.node(&target_node)?.clone();
        let activity = self.schedule_activity(dbx, ctx, &node, assignee_role.as_deref(), next_round, None).await?;
        let activity_id = parse_uuid(&activity.id)?;
        if !STORE.enter_returned_waiting(dbx, ctx.instance_id, activity_id).await? {
          return Err(FlowError::Conflict("instance left 'active' before the rework hand-back".into()));
        }
        self
          .append(
            dbx,
            ctx,
            event_type::RETURNED_TO_REWORK,
            json!({
              "from_node": source_activity.activity_definition_id,
              "rework_target": node.id,
              "round": next_round,
              "activity_id": activity.id,
              "assignee_role": assignee_role,
            }),
            None,
          )
          .await
      }
      AdvanceOutcome::Error => {
        self
          .fail_instance(
            dbx,
            ctx,
            &format!("no legal transition from '{}' (round {round})", source_activity.activity_definition_id),
          )
          .await
      }
    }
  }

  async fn fan_out(
    &self,
    dbx: &DbxPostgres,
    ctx: &mut InstanceCtx,
    targets: Vec<ParallelTarget>,
    trace: &ResolveTrace,
    source_activity: &ActivityInstance,
    round: i32,
  ) -> Result<()> {
    // No single current activity while branches run in parallel.
    STORE.clear_current_activity(dbx, ctx.instance_id).await?;

    let (via, join_target) = match &trace.for_each_fan_out {
      Some(fan) => {
        self
          .append(
            dbx,
            ctx,
            event_type::FOR_EACH_FANNED_OUT,
            json!({
              "for_each_node": fan.for_each_node,
              "join_node": fan.join_target,
              "branch_count": targets.len(),
              "round": round,
            }),
            None,
          )
          .await?;
        ("for_each", Some(fan.join_target.clone()))
      }
      None => {
        self
          .append(
            dbx,
            ctx,
            event_type::PARALLEL_SPLIT_SCHEDULED,
            json!({
              "from": source_activity.activity_definition_id,
              "branch_targets": targets.iter().map(|t| t.node_id.clone()).collect::<Vec<_>>(),
              "round": round,
            }),
            None,
          )
          .await?;
        ("parallel_split", None)
      }
    };

    for target in &targets {
      let node = ctx.node(&target.node_id)?.clone();
      self
        .schedule_activity(
          dbx,
          ctx,
          &node,
          target.assignee_role.as_deref(),
          round,
          Some(FanoutMark { via, item_index: target.item_index, item_payload: target.item_payload.as_ref() }),
        )
        .await?;
    }

    // Empty ForEach array is legal and MUST satisfy the join immediately, else the instance hangs.
    if targets.is_empty()
      && let Some(join_id) = join_target
    {
      let join_node = ctx.node(&join_id)?.clone();
      return Box::pin(self.try_satisfy_join(dbx, ctx, &join_node, source_activity, round, None)).await;
    }
    Ok(())
  }

  /// One branch reached the join: record the arrival, then re-evaluate the join策略.
  async fn branch_reached_join(
    &self,
    dbx: &DbxPostgres,
    ctx: &mut InstanceCtx,
    join_node: &WorkflowNode,
    source_activity: &ActivityInstance,
    round: i32,
  ) -> Result<()> {
    self
      .append(
        dbx,
        ctx,
        event_type::BRANCH_REACHED_JOIN,
        json!({
          "branch_node": source_activity.activity_definition_id,
          "join_node": join_node.id,
          "round": round,
        }),
        None,
      )
      .await?;
    let exclude = parse_uuid(&source_activity.id)?;
    self.try_satisfy_join(dbx, ctx, join_node, source_activity, round, Some(exclude)).await
  }

  async fn try_satisfy_join(
    &self,
    dbx: &DbxPostgres,
    ctx: &mut InstanceCtx,
    join_node: &WorkflowNode,
    source_activity: &ActivityInstance,
    round: i32,
    exclude_activity: Option<uuid::Uuid>,
  ) -> Result<()> {
    let strategy = join_node.join_strategy().unwrap_or(JoinStrategy::WaitAll);

    // Expected branch count: ForEach records its runtime fan-out, otherwise the static incoming
    // edge count is authoritative (framework gaps §2.2).
    let dynamic = STORE.for_each_branch_count(dbx, ctx.instance_id, &join_node.id, round).await?;
    let (expected, branch_ids) = match dynamic {
      Some(n) => {
        let template = ctx
          .snapshot
          .nodes
          .iter()
          .find_map(|node| node.for_each().and_then(|(_, tpl, jt, _)| (jt == join_node.id).then(|| tpl.to_string())))
          .ok_or_else(|| {
            FlowError::InvalidState(format!("join '{}' has a fan-out event but no ForEach source", join_node.id))
          })?;
        (n, vec![template])
      }
      None => {
        let ids: Vec<String> = ctx
          .snapshot
          .transitions
          .iter()
          .filter(|t| t.to == join_node.id && is_join_incoming_signal(t.on_signal))
          .map(|t| t.from.clone())
          .collect();
        (ids.len() as i64, ids)
      }
    };
    let arrived = STORE.count_completed_branches(dbx, ctx.instance_id, &branch_ids, round).await?;

    if !strategy.is_satisfied(arrived, expected) {
      // Still waiting on siblings — no single current activity.
      STORE.clear_current_activity(dbx, ctx.instance_id).await?;
      return Ok(());
    }

    self
      .append(
        dbx,
        ctx,
        event_type::PARALLEL_JOIN_SATISFIED,
        json!({ "join_node": join_node.id, "arrived": arrived, "expected": expected, "round": round }),
        None,
      )
      .await?;

    if strategy.skips_remaining_on_satisfaction() {
      let exclude = exclude_activity.unwrap_or_else(uuid::Uuid::nil);
      let skipped = STORE.skip_other_active_activities(dbx, ctx.instance_id, exclude).await?;
      if skipped > 0 {
        // No `activity_id` = "skip every still-active activity" in the reducer's vocabulary.
        self
          .append(dbx, ctx, event_type::ACTIVITY_SKIPPED, json!({ "join_node": join_node.id, "count": skipped }), None)
          .await?;
      }
    }

    let (outcome, trace) = decide_advance(
      &ctx.snapshot.nodes,
      &ctx.snapshot.transitions,
      &join_node.id,
      SignalType::Approved,
      ctx.context.as_ref(),
    );
    Box::pin(self.apply_outcome(dbx, ctx, outcome, &trace, source_activity, round)).await
  }

  async fn complete_instance(&self, dbx: &DbxPostgres, ctx: &mut InstanceCtx, result: WorkflowResult) -> Result<()> {
    if !STORE.complete_workflow_cas(dbx, ctx.instance_id, result.as_str()).await? {
      return Err(FlowError::Conflict("instance left 'active' before completion".into()));
    }
    self.append(dbx, ctx, event_type::COMPLETED, json!({ "result": result.as_str() }), None).await?;

    // Terminal business side effect: only on approval, and only as an outbox intent — the flag
    // flips after the handler confirms (framework spec §2 / §5).
    if result == WorkflowResult::Approved
      && let Some(tc) = self.terminal_callback(dbx, ctx).await?
    {
      let activity = STORE
        .active_activity(dbx, ctx.instance_id)
        .await?
        .or(STORE.list_activities(dbx, ctx.instance_id).await?.into_iter().next_back());
      let Some(anchor) = activity else {
        return Err(FlowError::InvalidState("terminal callback needs an activity anchor but none exists".into()));
      };
      let handler = self.registry.get(&tc.handler_type).ok_or_else(|| {
        FlowError::Validation(format!("terminal callback handler '{}' is not registered", tc.handler_type))
      })?;
      let idempotency_key = match handler.idempotency_key_strategy() {
        IdempotencyKeyStrategy::BusinessKey => format!("{}:{}", ctx.business_type, ctx.business_key),
        IdempotencyKeyStrategy::OutboxRecordId => ctx.instance_id.to_string(),
        IdempotencyKeyStrategy::PayloadField(field) => tc
          .payload
          .get(&field)
          .and_then(Value::as_str)
          .map(str::to_string)
          .unwrap_or_else(|| format!("{}:{}", ctx.business_type, ctx.business_key)),
      };
      let payload = BusinessCallbackPayload {
        tenant_id: ctx.tenant_id.to_string(),
        facility_id: ctx.facility_id.to_string(),
        workflow_instance_id: ctx.instance_id.to_string(),
        activity_instance_id: anchor.id.clone(),
        handler_type: tc.handler_type.clone(),
        idempotency_key,
        business_type: ctx.business_type.clone(),
        business_key: ctx.business_key.clone(),
        payload: tc.payload.clone(),
      };
      let value = serde_json::to_value(&payload)
        .map_err(|e| FlowError::Serialization(format!("business callback payload not serializable: {e}")))?;
      STORE
        .enqueue_outbox(
          dbx,
          ctx.tenant_id,
          ctx.facility_id,
          ctx.instance_id,
          parse_uuid(&anchor.id)?,
          outbox_activity::BUSINESS_CALLBACK,
          &value,
        )
        .await?;
    }
    Ok(())
  }

  /// The terminal callback intent lives in `workflow.started.v1` (the fact source), not in the
  /// business `context` a `context_writes` whitelist may rewrite.
  async fn terminal_callback(&self, dbx: &DbxPostgres, ctx: &InstanceCtx) -> Result<Option<TerminalCallback>> {
    let events = STORE.load_events(dbx, ctx.instance_id).await?;
    let Some(started) = events.iter().find(|e| e.event_type == event_type::STARTED) else { return Ok(None) };
    match started.payload.get(STARTED_TERMINAL_CALLBACK) {
      None | Some(Value::Null) => Ok(None),
      Some(value) => serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|e| FlowError::Serialization(format!("terminal_callback payload invalid: {e}"))),
    }
  }

  async fn fail_instance(&self, dbx: &DbxPostgres, ctx: &mut InstanceCtx, reason: &str) -> Result<()> {
    STORE.error_workflow(dbx, ctx.instance_id).await?;
    self.append(dbx, ctx, event_type::ERRORED, json!({ "reason": reason }), None).await
  }
}

// ===========================================================================
// helpers
// ===========================================================================

struct FanoutMark<'a> {
  via: &'a str,
  item_index: Option<i32>,
  item_payload: Option<&'a Value>,
}

/// Per-call instance facts every write needs (tenant / facility partition columns, the bound
/// snapshot, the live context).
struct InstanceCtx {
  instance_id: uuid::Uuid,
  tenant_id: i64,
  facility_id: uuid::Uuid,
  snapshot: DefinitionSnapshot,
  context: Option<Value>,
  business_type: String,
  business_key: String,
  reference_no: String,
}

impl InstanceCtx {
  fn node(&self, id: &str) -> Result<&WorkflowNode> {
    self
      .snapshot
      .nodes
      .iter()
      .find(|n| n.id == id)
      .ok_or_else(|| FlowError::InvalidState(format!("node '{id}' is not in the instance's bound snapshot")))
  }
}

/// Signal vocabulary must match the node kind, else the graph would advance on a signal its
/// author never declared.
fn check_signal_against_node(node: &WorkflowNode, signal: SignalType, event_type: Option<&str>) -> Result<()> {
  match signal {
    SignalType::Claimed | SignalType::Reassigned => match node.kind() {
      NodeKind::Approval | NodeKind::Assignment => Ok(()),
      other => Err(FlowError::Validation(format!("{signal:?} targets an approval/assignment node, got {other:?}"))),
    },
    SignalType::Approved | SignalType::Rejected | SignalType::Returned => match node.kind() {
      NodeKind::Approval => Ok(()),
      other => Err(FlowError::Validation(format!("{signal:?} targets an approval node, got {other:?}"))),
    },
    SignalType::Completed => match node.kind() {
      NodeKind::Notification | NodeKind::Assignment => Ok(()),
      other => Err(FlowError::Validation(format!("COMPLETED targets a notification/assignment node, got {other:?}"))),
    },
    SignalType::EventReceived => {
      let Some((declared, _, _)) = node.event_wait() else {
        return Err(FlowError::Validation(format!("EVENT_RECEIVED targets an event_wait node, got {:?}", node.kind())));
      };
      match event_type {
        Some(given) if given == declared => Ok(()),
        Some(given) => Err(FlowError::Validation(format!(
          "event_type '{given}' does not match node '{}' declared '{declared}'",
          node.id
        ))),
        None => Err(FlowError::Validation("EVENT_RECEIVED requires event_type".into())),
      }
    }
    SignalType::Resubmitted => {
      Err(FlowError::Validation("RESUBMITTED is not a signal — use WorkflowService::resubmit".into()))
    }
  }
}

/// Activity `result` column per signal. EventWait completion carries no result (the store's join
/// counter treats NULL as "arrived, not rejected").
fn signal_result(signal: SignalType) -> Option<&'static str> {
  match signal {
    SignalType::Approved => Some("approved"),
    SignalType::Rejected => Some("rejected"),
    SignalType::Returned => Some("returned"),
    SignalType::Completed => Some("completed"),
    _ => None,
  }
}

fn signal_str(signal: SignalType) -> &'static str {
  match signal {
    SignalType::Approved => "approved",
    SignalType::Returned => "returned",
    SignalType::Rejected => "rejected",
    SignalType::Resubmitted => "resubmitted",
    SignalType::EventReceived => "event_received",
    SignalType::Claimed => "claimed",
    SignalType::Reassigned => "reassigned",
    SignalType::Completed => "completed",
  }
}

pub(crate) fn parse_uuid(value: &str) -> Result<uuid::Uuid> {
  uuid::Uuid::parse_str(value).map_err(|_| FlowError::Validation(format!("invalid uuid: {value}")))
}

pub(crate) fn parse_i64(value: &str, field: &str) -> Result<i64> {
  value.parse::<i64>().map_err(|_| FlowError::Validation(format!("invalid {field}: {value}")))
}

#[cfg(test)]
mod tests {
  use hetuflow_core::{NodeConfig, WorkflowNode};

  use super::*;

  fn approval(id: &str) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: id.into(),
      context_writes: Vec::new(),
      config: NodeConfig::Approval {
        assignee_role: Some("r".into()),
        sla_seconds: None,
        escalation_seconds: None,
        escalation_target_role: None,
      },
    }
  }

  fn event_wait(id: &str) -> WorkflowNode {
    WorkflowNode {
      id: id.into(),
      name: id.into(),
      context_writes: Vec::new(),
      config: NodeConfig::EventWait {
        event_type: "ext.done".into(),
        timeout_seconds: Some(30),
        timeout_target: None,
        correlation_key: None,
        source: None,
      },
    }
  }

  #[test]
  fn approval_signals_require_an_approval_node() {
    assert!(check_signal_against_node(&approval("a"), SignalType::Approved, None).is_ok());
    assert!(check_signal_against_node(&event_wait("e"), SignalType::Approved, None).is_err());
  }

  #[test]
  fn event_received_matches_the_declared_event_type() {
    let node = event_wait("e");
    assert!(check_signal_against_node(&node, SignalType::EventReceived, Some("ext.done")).is_ok());
    assert!(check_signal_against_node(&node, SignalType::EventReceived, Some("other")).is_err());
    assert!(check_signal_against_node(&node, SignalType::EventReceived, None).is_err());
  }

  #[test]
  fn resubmitted_is_not_a_signal() {
    assert!(check_signal_against_node(&approval("a"), SignalType::Resubmitted, None).is_err());
  }

  #[test]
  fn signal_result_matches_the_store_join_counter_vocabulary() {
    assert_eq!(signal_result(SignalType::Approved), Some("approved"));
    assert_eq!(signal_result(SignalType::Completed), Some("completed"));
    // EventWait arrivals carry NULL so `count_completed_branches` counts them as arrived.
    assert_eq!(signal_result(SignalType::EventReceived), None);
  }

  #[test]
  fn round_limit_is_the_min_of_definition_and_framework_ceiling() {
    let service =
      WorkflowService::new(WorkflowConfig { max_round_hard_limit: 5, ..Default::default() }, CallbackRegistry::new());
    let mut ctx = InstanceCtx {
      instance_id: uuid::Uuid::nil(),
      tenant_id: 1,
      facility_id: uuid::Uuid::nil(),
      snapshot: DefinitionSnapshot {
        flow_type: "t".into(),
        version: 1,
        nodes: Vec::new(),
        transitions: Vec::new(),
        max_round: 2,
      },
      context: None,
      business_type: "b".into(),
      business_key: "k".into(),
      reference_no: "R".into(),
    };
    assert!(service.check_round(&ctx, 2).is_ok());
    assert!(service.check_round(&ctx, 3).is_err());
    // max_round = 0 → only the framework ceiling applies.
    ctx.snapshot.max_round = 0;
    assert!(service.check_round(&ctx, 5).is_ok());
    assert!(service.check_round(&ctx, 6).is_err());
  }

  #[test]
  fn registry_refuses_duplicate_handler_types() {
    struct H;
    impl hetuflow_core::BusinessCallbackHandler for H {
      fn handler_type(&self) -> &str {
        "demo"
      }
      fn execute<'a>(&'a self, _p: &'a BusinessCallbackPayload) -> hetuflow_core::CallbackFuture<'a> {
        Box::pin(async { Ok(()) })
      }
    }
    let mut registry = CallbackRegistry::new();
    assert!(registry.register(std::sync::Arc::new(H)).is_ok());
    assert!(registry.register(std::sync::Arc::new(H)).is_err());
    assert!(registry.contains("demo"));
  }
}
