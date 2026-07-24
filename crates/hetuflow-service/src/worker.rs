//! Reusable worker skeletons: the outbox dispatcher and the timer poller.
//!
//! Both are pure framework mechanics — lease, deliver, mark, back off — with every
//! application-specific decision behind a port ([`TxnRunner`] for transactions and session
//! context, [`NotificationDispatcher`] for delivery, [`CallbackRegistry`] for business handlers).
//!
//! ## Why two transactions per record
//!
//! Delivery is network I/O and MUST NOT happen inside a database transaction. So each record
//! crosses three phases:
//!
//! 1. **poll** (cross-tenant system transaction) — lease due rows with `FOR UPDATE SKIP LOCKED`;
//! 2. **deliver** (no transaction) — call the port;
//! 3. **settle** (the record's own tenant transaction) — mark succeeded / retry / dead-letter, and
//!    let the workflow advance under normal tenant isolation.
//!
//! A crash between 2 and 3 leaves a leased row whose lease expires and is redelivered — hence the
//! framework's at-least-once contract and the handler-side idempotency requirement
//! (`hetuflow.md` §4: "Outbox 保证投递状态可追踪，不承诺业务侧 exactly-once").

use std::sync::Arc;
use std::time::Duration;

use hetuflow_core::{
  BusinessCallbackPayload, CallbackError, CallbackErrorKind, NotificationPayload, OutboxRecord, Result,
  retry_backoff_secs,
};
use hetuflow_sqlx::{PgWorkflowStore, WorkflowStore};

use crate::ports::{CallbackRegistry, NotificationDispatcher, TxnRunner};
use crate::service::{WorkflowService, parse_i64, parse_uuid};

const STORE: PgWorkflowStore = PgWorkflowStore::new();

/// Tuning knobs for both loops.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
  /// Stable id of this worker process (owns the outbox lease).
  pub worker_id: String,
  /// How long a leased outbox row stays owned before another worker may steal it.
  pub lease_ttl_secs: i32,
  /// Rows claimed per pass.
  pub batch_size: i64,
  /// Sleep between passes.
  pub poll_interval: Duration,
  /// Exponential backoff base for transient delivery failures.
  pub retry_base_secs: i64,
  /// Backoff ceiling.
  pub retry_cap_secs: i64,
}

impl Default for WorkerConfig {
  fn default() -> Self {
    Self {
      worker_id: "hetuflow-worker".to_string(),
      lease_ttl_secs: 60,
      batch_size: 32,
      poll_interval: Duration::from_secs(5),
      retry_base_secs: 10,
      retry_cap_secs: 300,
    }
  }
}

/// Outbox dispatcher: leases due side-effect intents, delivers them through the adapter's ports,
/// and settles the result.
pub struct OutboxDispatcher<R: TxnRunner, N: NotificationDispatcher> {
  runner: Arc<R>,
  notifications: Arc<N>,
  service: Arc<WorkflowService>,
  registry: CallbackRegistry,
  config: WorkerConfig,
}

impl<R: TxnRunner, N: NotificationDispatcher> OutboxDispatcher<R, N> {
  pub fn new(
    runner: Arc<R>,
    notifications: Arc<N>,
    service: Arc<WorkflowService>,
    registry: CallbackRegistry,
    config: WorkerConfig,
  ) -> Self {
    Self { runner, notifications, service, registry, config }
  }

  /// One full pass. Returns how many records were settled (delivered or failed), so callers can
  /// drive it deterministically from tests instead of waiting on the loop.
  pub async fn run_once(&self) -> Result<usize> {
    let worker_id = self.config.worker_id.clone();
    let lease_ttl = self.config.lease_ttl_secs;
    let batch = self.config.batch_size;
    let records: Vec<OutboxRecord> = self
      .runner
      .system_write("hetuflow:outbox_poll", move |dbx| async move {
        STORE.poll_due_outbox(&dbx, &worker_id, lease_ttl, batch).await
      })
      .await?;

    let mut settled = 0usize;
    for record in records {
      match self.deliver(&record).await {
        Ok(()) => {
          self.settle_success(&record).await?;
          settled += 1;
        }
        Err(error) => {
          self.settle_failure(&record, &error).await?;
          settled += 1;
        }
      }
    }
    Ok(settled)
  }

  /// Run forever on `poll_interval`. A failing pass is logged and retried next tick — a poller
  /// that dies on a transient database error would silently stop every workflow's side effects.
  pub async fn run_forever(self) {
    let mut ticker = tokio::time::interval(self.config.poll_interval.max(Duration::from_millis(100)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
      ticker.tick().await;
      if let Err(e) = self.run_once().await {
        tracing::warn!(error = %e, "hetuflow outbox dispatcher pass failed");
      }
    }
  }

  async fn deliver(&self, record: &OutboxRecord) -> std::result::Result<(), CallbackError> {
    match record.activity_type.as_str() {
      hetuflow_core::outbox_activity::NOTIFICATION
      | hetuflow_core::outbox_activity::NOTIFICATION_REMINDER
      | hetuflow_core::outbox_activity::NOTIFICATION_ESCALATION => {
        let payload: NotificationPayload = serde_json::from_value(record.payload.clone())
          .map_err(|e| CallbackError::deterministic(format!("notification payload invalid: {e}")))?;
        self.notifications.deliver(&payload).await
      }
      hetuflow_core::outbox_activity::BUSINESS_CALLBACK => {
        let payload: BusinessCallbackPayload = serde_json::from_value(record.payload.clone())
          .map_err(|e| CallbackError::deterministic(format!("business callback payload invalid: {e}")))?;
        let Some(handler) = self.registry.get(&payload.handler_type) else {
          // Deterministic: retrying cannot conjure a handler into the registry.
          return Err(CallbackError::deterministic(format!(
            "no business callback handler registered for '{}'",
            payload.handler_type
          )));
        };
        handler.execute(&payload).await
      }
      other => Err(CallbackError::deterministic(format!("unknown outbox activity_type '{other}'"))),
    }
  }

  async fn settle_success(&self, record: &OutboxRecord) -> Result<()> {
    let tenant_id = parse_i64(&record.tenant_id, "tenant_id")?;
    let record_id = parse_uuid(&record.id)?;
    let instance_id = parse_uuid(&record.workflow_instance_id)?;
    let activity_id = parse_uuid(&record.activity_instance_id)?;
    let worker_id = self.config.worker_id.clone();
    let activity_type = record.activity_type.clone();
    let service = self.service.clone();

    self
      .runner
      .tenant_write(tenant_id, "hetuflow:outbox_settle", move |dbx| async move {
        if !STORE.mark_outbox_succeeded(&dbx, record_id, &worker_id).await? {
          // Lease stolen after delivery: another worker owns the follow-up. Delivering twice is
          // the documented at-least-once cost; advancing twice is not, so stop here.
          return Ok(());
        }
        match activity_type.as_str() {
          hetuflow_core::outbox_activity::NOTIFICATION => {
            service.on_notification_delivered(&dbx, instance_id, activity_id).await
          }
          hetuflow_core::outbox_activity::BUSINESS_CALLBACK => {
            service.on_business_callback_delivered(&dbx, instance_id).await
          }
          // reminder / escalation notifications are pure nudges: delivery does not move the graph.
          _ => Ok(()),
        }
      })
      .await
  }

  async fn settle_failure(&self, record: &OutboxRecord, error: &CallbackError) -> Result<()> {
    let tenant_id = parse_i64(&record.tenant_id, "tenant_id")?;
    let record_id = parse_uuid(&record.id)?;
    let instance_id = parse_uuid(&record.workflow_instance_id)?;
    let activity_id = parse_uuid(&record.activity_instance_id)?;
    let worker_id = self.config.worker_id.clone();
    let activity_type = record.activity_type.clone();
    let message = error.message.clone();
    let service = self.service.clone();

    // A deterministic failure will fail identically forever; only transient ones get the budget.
    let exhausted = record.attempt_count + 1 >= record.max_attempts;
    let dead = matches!(error.kind, CallbackErrorKind::Deterministic) || exhausted;
    let backoff = retry_backoff_secs(record.attempt_count, self.config.retry_base_secs, self.config.retry_cap_secs);

    self
      .runner
      .tenant_write(tenant_id, "hetuflow:outbox_settle", move |dbx| async move {
        if dead {
          if STORE.mark_outbox_dead_letter(&dbx, record_id, &worker_id, &message).await? {
            service.on_outbox_dead_letter(&dbx, instance_id, activity_id, &activity_type, &message).await?;
          }
        } else {
          STORE.mark_outbox_retry(&dbx, record_id, &worker_id, backoff as i32, &message).await?;
        }
        Ok(())
      })
      .await
  }
}

/// Timer poller: fires due durable timers (SLA reminder / approval escalation / EventWait timeout).
pub struct TimerPoller<R: TxnRunner> {
  runner: Arc<R>,
  service: Arc<WorkflowService>,
  config: WorkerConfig,
}

impl<R: TxnRunner> TimerPoller<R> {
  pub fn new(runner: Arc<R>, service: Arc<WorkflowService>, config: WorkerConfig) -> Self {
    Self { runner, service, config }
  }

  /// One pass: claim due timers, then fire each in its own tenant transaction.
  ///
  /// The claim transaction only *reads* (`FOR UPDATE SKIP LOCKED` without a status flip), so a
  /// timer can be handed to two passes; `fire_timer` closes that with a `status = 'pending'` CAS
  /// on `mark_timer_fired` and returns `false` for the loser.
  pub async fn run_once(&self) -> Result<usize> {
    let batch = self.config.batch_size;
    let due = self
      .runner
      .system_write("hetuflow:timer_poll", move |dbx| async move { STORE.poll_due_timers(&dbx, batch).await })
      .await?;

    let mut fired = 0usize;
    for timer in due {
      let tenant_id = parse_i64(&timer.tenant_id, "tenant_id")?;
      let service = self.service.clone();
      let timer_for_txn = timer.clone();
      let ok = self
        .runner
        .tenant_write(tenant_id, "hetuflow:timer_fire", move |dbx| async move {
          service.fire_timer(&dbx, &timer_for_txn).await
        })
        .await;
      match ok {
        Ok(true) => fired += 1,
        Ok(false) => {}
        // One poisoned timer must not stall the rest of the queue.
        Err(e) => tracing::warn!(timer_id = %timer.id, error = %e, "hetuflow timer fire failed"),
      }
    }
    Ok(fired)
  }

  pub async fn run_forever(self) {
    let mut ticker = tokio::time::interval(self.config.poll_interval.max(Duration::from_millis(100)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
      ticker.tick().await;
      if let Err(e) = self.run_once().await {
        tracing::warn!(error = %e, "hetuflow timer poller pass failed");
      }
    }
  }
}
