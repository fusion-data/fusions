//! hetuflow-sqlx —— 工作流执行态的 Postgres 存储。
//!
//! ## API surface
//!
//! 业务方通过 [`PgWorkflowStore`] + [`WorkflowStore`] trait 调用：
//!
//! ```ignore
//! use hetuflow::sqlx::{PgWorkflowStore, WorkflowStore};
//! const STORE: PgWorkflowStore = PgWorkflowStore;
//! STORE.insert_activity(&dbx, ...).await?;
//! ```
//!
//! ## A1 invariant（§16.1）
//!
//! Trait 方法接 caller 传入、已在事务内的 `&DbxPostgres`。本 crate **不**自开事务、
//! **不**做 GUC 注入 —— `SET LOCAL app.*`（RLS/audit）注入是 caller `with_*_txn` 块
//! 的唯一职责。
//!
//! ## 为什么 trait 是 PG-only（Phase H-c 锁定）
//!
//! 真正多态的 `WorkflowBackend`（Restate/Cloacina/...）要求 backend 自拥事务生命
//! 周期 —— 与 A1 冲突，故 §16.4 锁定为 NOT in scope。此 trait 仅做**契约文档化**：
//! 列出 hylx-task 对 store 的能力集，便于未来评估替代实现 + framework-level 测试
//! helper（如果真要做第二个 backend 候选，先去松 A1 invariant 再扩抽象）。

use fusionsql::store::DbxPostgres;
use hetuflow_core::{
  ActivityInstance, ActivityType, DefinitionSnapshot, EventRecord, FacilityVisibility, FlowError, OutboxRecord, Result,
  ScopeFilter, TimerRecord, WorkflowInstance, WorkflowResult, WorkflowStatus,
};
use sqlx::{Postgres, QueryBuilder};

const INSTANCE_COLS: &str = "id, tenant_id, facility_id, reference_no, workflow_definition_id, business_key, business_type, status, current_activity_id, result, side_effects_executed, context, started_at, completed_at, created_at, updated_at";
const ACTIVITY_COLS: &str = "id, tenant_id, workflow_instance_id, activity_definition_id, activity_type, status, assignee_role, assignee_id, result, review_notes, reviewed_by, reviewed_at, round";
const OUTBOX_COLS: &str = "id, tenant_id, facility_id, workflow_instance_id, activity_instance_id, activity_type, payload, status, attempt_count, max_attempts, next_attempt_at, last_error, created_at, updated_at";

fn parse_uuid(s: &str, field: &str) -> Result<uuid::Uuid> {
  uuid::Uuid::parse_str(s).map_err(|_| FlowError::Validation(format!("invalid uuid for {field}: {s}")))
}

fn map_db<E: std::fmt::Display>(e: E) -> FlowError {
  FlowError::Storage(e.to_string())
}

/// 把 `ScopeFilter` 推成 ` AND tenant_id = .. [AND facility_id = ANY(..)]`（脱 TaskIdentityScope）。
fn push_scope(qb: &mut QueryBuilder<Postgres>, scope: &ScopeFilter) -> Result<()> {
  qb.push(" AND tenant_id = ");
  qb.push_bind(
    scope
      .tenant_id
      .parse::<i64>()
      .map_err(|_| FlowError::Validation(format!("invalid tenant_id: {}", scope.tenant_id)))?,
  );
  if let FacilityVisibility::Only(ids) = &scope.facilities {
    let mut uuids = Vec::with_capacity(ids.len());
    for s in ids {
      uuids.push(parse_uuid(s, "facility_id")?);
    }
    qb.push(" AND facility_id = ANY(");
    qb.push_bind(uuids);
    qb.push(")");
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// 私有 Row 类型
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct InstanceRow {
  id: uuid::Uuid,
  tenant_id: i64,
  facility_id: uuid::Uuid,
  reference_no: String,
  workflow_definition_id: uuid::Uuid,
  business_key: String,
  business_type: String,
  status: String,
  current_activity_id: Option<uuid::Uuid>,
  result: Option<String>,
  side_effects_executed: bool,
  context: Option<serde_json::Value>,
  started_at: Option<chrono::DateTime<chrono::Utc>>,
  completed_at: Option<chrono::DateTime<chrono::Utc>>,
  created_at: chrono::DateTime<chrono::Utc>,
  updated_at: chrono::DateTime<chrono::Utc>,
}

impl InstanceRow {
  fn into_model(self) -> WorkflowInstance {
    WorkflowInstance {
      id: self.id.to_string(),
      tenant_id: self.tenant_id.to_string(),
      facility_id: self.facility_id.to_string(),
      reference_no: self.reference_no,
      workflow_definition_id: self.workflow_definition_id.to_string(),
      business_key: self.business_key,
      business_type: self.business_type,
      status: WorkflowStatus::from_db(&self.status),
      current_activity_id: self.current_activity_id.map(|id| id.to_string()),
      result: self.result.and_then(|r| WorkflowResult::from_db(&r)),
      side_effects_executed: self.side_effects_executed,
      context: self.context,
      started_at: self.started_at,
      completed_at: self.completed_at,
      created_at: self.created_at,
      updated_at: self.updated_at,
    }
  }
}

#[derive(sqlx::FromRow)]
struct ActivityRow {
  id: uuid::Uuid,
  tenant_id: i64,
  workflow_instance_id: uuid::Uuid,
  activity_definition_id: String,
  activity_type: String,
  status: String,
  assignee_role: Option<String>,
  assignee_id: Option<i64>,
  result: Option<String>,
  review_notes: Option<String>,
  reviewed_by: Option<i64>,
  reviewed_at: Option<chrono::DateTime<chrono::Utc>>,
  round: i32,
}

impl ActivityRow {
  fn into_model(self) -> ActivityInstance {
    ActivityInstance {
      id: self.id.to_string(),
      tenant_id: self.tenant_id.to_string(),
      workflow_instance_id: self.workflow_instance_id.to_string(),
      activity_definition_id: self.activity_definition_id,
      activity_type: ActivityType::from_db(&self.activity_type),
      status: hetuflow_core::ActivityStatus::from_db(&self.status),
      assignee_role: self.assignee_role,
      assignee_id: self.assignee_id.map(|id| id.to_string()),
      result: self.result,
      review_notes: self.review_notes,
      reviewed_by: self.reviewed_by.map(|id| id.to_string()),
      reviewed_at: self.reviewed_at,
      round: self.round,
    }
  }
}

#[derive(sqlx::FromRow)]
struct OutboxRow {
  id: uuid::Uuid,
  tenant_id: i64,
  facility_id: uuid::Uuid,
  workflow_instance_id: uuid::Uuid,
  activity_instance_id: uuid::Uuid,
  activity_type: String,
  payload: serde_json::Value,
  status: String,
  attempt_count: i32,
  max_attempts: i32,
  next_attempt_at: chrono::DateTime<chrono::Utc>,
  last_error: Option<String>,
  created_at: chrono::DateTime<chrono::Utc>,
  updated_at: chrono::DateTime<chrono::Utc>,
}

impl OutboxRow {
  fn into_model(self) -> OutboxRecord {
    OutboxRecord {
      id: self.id.to_string(),
      tenant_id: self.tenant_id.to_string(),
      facility_id: self.facility_id.to_string(),
      workflow_instance_id: self.workflow_instance_id.to_string(),
      activity_instance_id: self.activity_instance_id.to_string(),
      activity_type: self.activity_type,
      payload: self.payload,
      status: self.status,
      attempt_count: self.attempt_count,
      max_attempts: self.max_attempts,
      next_attempt_at: self.next_attempt_at,
      last_error: self.last_error,
      created_at: self.created_at,
      updated_at: self.updated_at,
    }
  }
}

#[derive(sqlx::FromRow)]
struct TimerRow {
  id: uuid::Uuid,
  tenant_id: i64,
  facility_id: uuid::Uuid,
  workflow_instance_id: uuid::Uuid,
  activity_instance_id: uuid::Uuid,
  /// Phase F2：'reminder' | 'escalation'，worker 据此路由不同 fire 处理
  timer_kind: String,
}

impl TimerRow {
  fn into_record(self) -> TimerRecord {
    TimerRecord {
      id: self.id.to_string(),
      tenant_id: self.tenant_id.to_string(),
      facility_id: self.facility_id.to_string(),
      workflow_instance_id: self.workflow_instance_id.to_string(),
      activity_instance_id: self.activity_instance_id.to_string(),
      timer_kind: self.timer_kind,
    }
  }
}

// ---------------------------------------------------------------------------
// WorkflowStore 契约（PG-only；trait 仅文档化能力集 + 未来扩抽象的入口）
// ---------------------------------------------------------------------------

/// Workflow 执行态存储契约。
///
/// **A1 invariant**：所有方法接 caller 传入的 `&DbxPostgres`（已在事务内），实现
/// **不**自开事务、**不**做 GUC 注入。当前唯一 in-tree 实现：[`PgWorkflowStore`]。
///
/// `async fn` 不显式标 `+ Send` 因 trait 仅供 hylx-task 内部 + 唯一 concrete impl
/// （`PgWorkflowStore` over sqlx + Postgres，future 天然 Send）；generic dispatch
/// 未来若需要再 desugar 为 `-> impl Future + Send`。
#[allow(async_fn_in_trait)]
pub trait WorkflowStore: Send + Sync {
  // ============ WorkflowInstance ============

  /// 启动实例 —— INSERT ON CONFLICT DO NOTHING（uq_workflow_instances_active 去重）。
  /// `Some` = 新建；`None` = 已存在 active 实例。snapshot 由 caller 序列化（core serde）。
  #[allow(clippy::too_many_arguments)]
  async fn insert_instance_on_conflict(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    definition_id: uuid::Uuid,
    reference_no: &str,
    business_key: &str,
    business_type: &str,
    context: Option<&serde_json::Value>,
    snapshot: &serde_json::Value,
    hash: &str,
  ) -> Result<Option<WorkflowInstance>>;

  async fn active_instance_by_business_key(
    &self,
    dbx: &DbxPostgres,
    business_key: &str,
    business_type: &str,
  ) -> Result<Option<WorkflowInstance>>;

  /// scope-filtered 读实例（RPC GetWorkflowInstance）。
  async fn get_instance_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    id: uuid::Uuid,
  ) -> Result<Option<WorkflowInstance>>;

  /// FOR UPDATE 行锁载入实例（scope-filtered）—— CQ1 单写者。
  async fn load_instance_for_update_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    id: uuid::Uuid,
  ) -> Result<Option<WorkflowInstance>>;

  /// 按 id 读实例（系统态、无 scope、无锁；TimerScanner 用）。
  async fn load_instance(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<Option<WorkflowInstance>>;

  /// 读实例绑定的不可变 definition 快照（core serde）。
  async fn load_snapshot(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<DefinitionSnapshot>;

  async fn complete_workflow_cas(&self, dbx: &DbxPostgres, id: uuid::Uuid, result: &str) -> Result<bool>;

  async fn reactivate_workflow(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool>;

  async fn error_workflow(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool>;

  /// Phase G1：清空 current_activity_id（并行多 active 时无单一 current 语义）。
  async fn clear_current_activity(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<()>;

  async fn update_current_activity(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
  ) -> Result<bool>;

  /// 缺口 A 通用 rework loop（精确返工）：active → returned_waiting 退回，
  /// `current_activity_id` 指向新建的 ReworkTarget 活动。CAS 仅在 status='active' 时生效。
  async fn enter_returned_waiting(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    new_activity_id: uuid::Uuid,
  ) -> Result<bool>;

  /// 缺口 A 通用 rework loop：returned_waiting → active 重入主链（ReworkTarget 通过后）。
  /// CAS 仅在 status='returned_waiting' 时生效；resume 后 `update_current_activity` 才可用。
  async fn resume_from_returned_waiting(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool>;

  async fn mark_side_effects_executed(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool>;

  /// 运行期 context 写回：把已校验的 patch merge 到 instance.context，并返回 patched context。
  /// caller 必须先持有 workflow_instances FOR UPDATE 行锁并完成 context_writes 白名单校验。
  async fn merge_instance_context(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    patch: &serde_json::Value,
  ) -> Result<serde_json::Value>;

  // ============ ActivityInstance ============

  /// 插入审批活动（active）。返回新建 activity（含 DB 生成 id）。
  async fn insert_activity(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    instance_id: uuid::Uuid,
    node_id: &str,
    activity_type: ActivityType,
    assignee_role: Option<&str>,
    round: i32,
  ) -> Result<ActivityInstance>;

  async fn active_activity(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Option<ActivityInstance>>;

  /// Phase G1：列实例内全部 active 活动（多 active 并行分支用）。
  async fn active_activities(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Vec<ActivityInstance>>;

  /// Phase G1：统计某 Join 节点在某 round 内已 completed 的分支数（用于 wait_all 判定）。
  /// 计数标准：activity_definition_id 在 branch_node_ids 列表内 + status='completed' + result='approved'。
  async fn count_completed_branches(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    branch_node_ids: &[String],
    round: i32,
  ) -> Result<i64>;

  /// G2 ForEach（§2.2 BranchExpectation::Dynamic）：读某 round 内指向 `join_node` 的
  /// `FOR_EACH_FANNED_OUT` 事件记录的 `branch_count`。返回 `Some(n)` 表示本 round 由 ForEach
  /// 动态扇出（join 计数用 Dynamic(n)）；`None` 表示无（join 计数回退 Static 静态边数）。
  async fn for_each_branch_count(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    join_node: &str,
    round: i32,
  ) -> Result<Option<i64>>;

  async fn load_activity(&self, dbx: &DbxPostgres, activity_id: uuid::Uuid) -> Result<Option<ActivityInstance>>;

  /// 列出实例全部活动（按 created_at；RPC GetWorkflowInstance.include_activities）。
  async fn list_activities(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Vec<ActivityInstance>>;

  async fn complete_activity_cas(
    &self,
    dbx: &DbxPostgres,
    activity_id: uuid::Uuid,
    result: Option<&str>,
    reviewed_by: i64,
    review_notes: Option<&str>,
  ) -> Result<bool>;

  /// Phase F1：EventWait 活动超时跳过（status active → skipped）。
  async fn skip_activity_for_timeout(&self, dbx: &DbxPostgres, activity_id: uuid::Uuid) -> Result<bool>;

  /// Phase G1：把实例内除指定 activity 外的全部 active 活动标记为 skipped（fail-fast）。
  /// 同事务取消这些活动的 pending timer。返跳过的活动数。
  async fn skip_other_active_activities(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    exclude_activity_id: uuid::Uuid,
  ) -> Result<u64>;

  async fn max_round(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<i32>;

  // ============ Event log ============

  async fn next_seq(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<i64>;

  /// 读实例全部事件（按 seq 升序）—— projection rebuild / 审计追溯的读路径。
  /// 系统态、无 scope filter（caller 在 RPC/CLI 层做权限与 scope 门控）。
  async fn load_events(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Vec<EventRecord>>;

  /// scope-filtered 列实例 id（drift 批量核验）；`terminal_only` 限 completed/errored/cancelled。
  async fn list_instance_ids_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    terminal_only: bool,
    limit: i64,
  ) -> Result<Vec<uuid::Uuid>>;

  async fn idempotency_seen(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid, key: &str) -> Result<bool>;

  #[allow(clippy::too_many_arguments)]
  async fn append_event(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    instance_id: uuid::Uuid,
    seq: i64,
    event_type: &str,
    payload: serde_json::Value,
    idempotency_key: Option<&str>,
  ) -> Result<()>;

  // ============ Activity Outbox ============

  #[allow(clippy::too_many_arguments)]
  async fn enqueue_outbox(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
    activity_type: &str,
    payload: &serde_json::Value,
  ) -> Result<()>;

  async fn poll_due_outbox(
    &self,
    dbx: &DbxPostgres,
    worker_id: &str,
    lease_ttl_secs: i32,
    limit: i64,
  ) -> Result<Vec<OutboxRecord>>;

  async fn mark_outbox_succeeded(&self, dbx: &DbxPostgres, id: uuid::Uuid, worker_id: &str) -> Result<bool>;

  async fn mark_outbox_retry(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    worker_id: &str,
    backoff_secs: i32,
    last_error: &str,
  ) -> Result<bool>;

  async fn mark_outbox_dead_letter(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    worker_id: &str,
    last_error: &str,
  ) -> Result<bool>;

  async fn list_outbox_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    instance_id: uuid::Uuid,
  ) -> Result<Vec<OutboxRecord>>;

  // ============ Timer ============

  /// Phase F2：`timer_kind` 区分 'reminder' / 'escalation'，同一活动允许两条 pending timer
  /// （reminder + escalation 并存）。partial UNIQUE 索引 (instance, activity, timer_kind)
  /// 保证同 kind 至多一条 pending。
  #[allow(clippy::too_many_arguments)]
  async fn schedule_timer(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
    timer_kind: &str,
    fire_in_seconds: i32,
  ) -> Result<()>;

  /// 取消活动的全部 pending timers（reminder + escalation），活动 complete/skip 时调用。
  async fn cancel_timers_for_activity(&self, dbx: &DbxPostgres, activity_id: uuid::Uuid) -> Result<()>;

  async fn poll_due_timers(&self, dbx: &DbxPostgres, limit: i64) -> Result<Vec<TimerRecord>>;

  /// Phase F2：升级活动的 assignee_role（CAS：仅 active 状态更新）。
  /// 返 true = 更新成功；false = 活动已非 active（并发完成）。
  async fn update_activity_assignee_role(
    &self,
    dbx: &DbxPostgres,
    activity_id: uuid::Uuid,
    new_role: &str,
  ) -> Result<bool>;

  async fn mark_timer_fired(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool>;
}

// ---------------------------------------------------------------------------
// PgWorkflowStore —— Postgres-direct 实现
// ---------------------------------------------------------------------------

/// Postgres-direct workflow store（unit struct；零容量）。
///
/// 业务侧推荐挂为模块顶层 const：
/// ```ignore
/// const STORE: PgWorkflowStore = PgWorkflowStore;
/// STORE.insert_activity(&dbx, ...).await?;
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct PgWorkflowStore;

impl PgWorkflowStore {
  pub const fn new() -> Self {
    Self
  }
}

impl WorkflowStore for PgWorkflowStore {
  // ============ WorkflowInstance ============

  async fn insert_instance_on_conflict(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    definition_id: uuid::Uuid,
    reference_no: &str,
    business_key: &str,
    business_type: &str,
    context: Option<&serde_json::Value>,
    snapshot: &serde_json::Value,
    hash: &str,
  ) -> Result<Option<WorkflowInstance>> {
    let row = dbx
      .fetch_optional(
        sqlx::query_as::<_, InstanceRow>(&format!(
          "INSERT INTO workflow_instances (tenant_id, facility_id, reference_no, workflow_definition_id, business_key, business_type, status, side_effects_executed, context, definition_snapshot, definition_hash, started_at)
           VALUES ($1, $2, $3, $4, $5, $6, 'active', false, $7, $8, $9, now())
           ON CONFLICT (business_key, business_type) WHERE status IN ('pending', 'active') DO NOTHING
           RETURNING {INSTANCE_COLS}"
        ))
        .bind(tenant_id)
        .bind(facility_id)
        .bind(reference_no)
        .bind(definition_id)
        .bind(business_key)
        .bind(business_type)
        .bind(context)
        .bind(snapshot)
        .bind(hash),
      )
      .await
      .map_err(map_db)?;
    Ok(row.map(|r| r.into_model()))
  }

  async fn active_instance_by_business_key(
    &self,
    dbx: &DbxPostgres,
    business_key: &str,
    business_type: &str,
  ) -> Result<Option<WorkflowInstance>> {
    let row = dbx
      .fetch_optional(
        sqlx::query_as::<_, InstanceRow>(&format!(
          "SELECT {INSTANCE_COLS} FROM workflow_instances WHERE business_key = $1 AND business_type = $2 AND status IN ('pending', 'active') LIMIT 1"
        ))
        .bind(business_key)
        .bind(business_type),
      )
      .await
      .map_err(map_db)?;
    Ok(row.map(|r| r.into_model()))
  }

  async fn get_instance_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    id: uuid::Uuid,
  ) -> Result<Option<WorkflowInstance>> {
    let mut qb: QueryBuilder<Postgres> =
      QueryBuilder::new(format!("SELECT {INSTANCE_COLS} FROM workflow_instances WHERE id = "));
    qb.push_bind(id);
    push_scope(&mut qb, scope)?;
    let row = dbx.fetch_optional(qb.build_query_as::<InstanceRow>()).await.map_err(map_db)?;
    Ok(row.map(|r| r.into_model()))
  }

  async fn load_instance_for_update_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    id: uuid::Uuid,
  ) -> Result<Option<WorkflowInstance>> {
    let mut qb: QueryBuilder<Postgres> =
      QueryBuilder::new(format!("SELECT {INSTANCE_COLS} FROM workflow_instances WHERE id = "));
    qb.push_bind(id);
    push_scope(&mut qb, scope)?;
    qb.push(" FOR UPDATE");
    let row = dbx.fetch_optional(qb.build_query_as::<InstanceRow>()).await.map_err(map_db)?;
    Ok(row.map(|r| r.into_model()))
  }

  async fn load_instance(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<Option<WorkflowInstance>> {
    let row = dbx
      .fetch_optional(
        sqlx::query_as::<_, InstanceRow>(&format!("SELECT {INSTANCE_COLS} FROM workflow_instances WHERE id = $1"))
          .bind(id),
      )
      .await
      .map_err(map_db)?;
    Ok(row.map(|r| r.into_model()))
  }

  async fn load_snapshot(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<DefinitionSnapshot> {
    let row: (serde_json::Value,) = dbx
      .fetch_one(sqlx::query_as("SELECT definition_snapshot FROM workflow_instances WHERE id = $1").bind(id))
      .await
      .map_err(map_db)?;
    serde_json::from_value(row.0).map_err(|e| FlowError::Serialization(format!("definition_snapshot invalid: {e}")))
  }

  async fn complete_workflow_cas(&self, dbx: &DbxPostgres, id: uuid::Uuid, result: &str) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_instances SET status = 'completed', result = $1, completed_at = now(), updated_at = now(), current_activity_id = NULL WHERE id = $2 AND status = 'active'",
        )
        .bind(result)
        .bind(id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn reactivate_workflow(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_instances SET status = 'active', result = NULL, completed_at = NULL, updated_at = now() WHERE id = $1 AND status = 'completed' AND result = 'returned'",
        )
        .bind(id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn error_workflow(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_instances SET status = 'errored', updated_at = now() WHERE id = $1 AND status = 'active'",
        )
        .bind(id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn clear_current_activity(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<()> {
    dbx
      .execute(
        sqlx::query("UPDATE workflow_instances SET current_activity_id = NULL, updated_at = now() WHERE id = $1")
          .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(())
  }

  async fn update_current_activity(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
  ) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_instances SET current_activity_id = $1, updated_at = now() WHERE id = $2 AND status = 'active'",
        )
        .bind(activity_id)
        .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn enter_returned_waiting(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    new_activity_id: uuid::Uuid,
  ) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_instances SET status = 'returned_waiting', current_activity_id = $2, updated_at = now() WHERE id = $1 AND status = 'active'",
        )
        .bind(id)
        .bind(new_activity_id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn resume_from_returned_waiting(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_instances SET status = 'active', updated_at = now() WHERE id = $1 AND status = 'returned_waiting'",
        )
        .bind(id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn mark_side_effects_executed(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_instances SET side_effects_executed = true, updated_at = now() WHERE id = $1 AND side_effects_executed = false",
        )
        .bind(id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn merge_instance_context(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    patch: &serde_json::Value,
  ) -> Result<serde_json::Value> {
    let row: (serde_json::Value,) = dbx
      .fetch_one(
        sqlx::query_as(
          "UPDATE workflow_instances SET context = COALESCE(context, '{}'::jsonb) || $2::jsonb, updated_at = now() \
           WHERE id = $1 RETURNING context",
        )
        .bind(id)
        .bind(patch),
      )
      .await
      .map_err(map_db)?;
    Ok(row.0)
  }

  // ============ ActivityInstance ============

  async fn insert_activity(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    instance_id: uuid::Uuid,
    node_id: &str,
    activity_type: ActivityType,
    assignee_role: Option<&str>,
    round: i32,
  ) -> Result<ActivityInstance> {
    let row = dbx
      .fetch_one(
        sqlx::query_as::<_, ActivityRow>(&format!(
          "INSERT INTO activity_instances (tenant_id, workflow_instance_id, activity_definition_id, activity_type, status, assignee_role, assignee_id, result, round)
           VALUES ($1, $2, $3, $4, 'active', $5, NULL, NULL, $6)
           RETURNING {ACTIVITY_COLS}"
        ))
        .bind(tenant_id)
        .bind(instance_id)
        .bind(node_id)
        .bind(activity_type.as_str())
        .bind(assignee_role)
        .bind(round),
      )
      .await
      .map_err(map_db)?;
    Ok(row.into_model())
  }

  async fn active_activity(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Option<ActivityInstance>> {
    let row = dbx
      .fetch_optional(
        sqlx::query_as::<_, ActivityRow>(&format!(
          "SELECT {ACTIVITY_COLS} FROM activity_instances WHERE workflow_instance_id = $1 AND status = 'active' ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(row.map(|r| r.into_model()))
  }

  async fn active_activities(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Vec<ActivityInstance>> {
    let rows = dbx
      .fetch_all(
        sqlx::query_as::<_, ActivityRow>(&format!(
          "SELECT {ACTIVITY_COLS} FROM activity_instances WHERE workflow_instance_id = $1 AND status = 'active' ORDER BY created_at ASC"
        ))
        .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(rows.into_iter().map(|r| r.into_model()).collect())
  }

  async fn count_completed_branches(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    branch_node_ids: &[String],
    round: i32,
  ) -> Result<i64> {
    if branch_node_ids.is_empty() {
      return Ok(0);
    }
    // 「到达 join」= 分支活动 completed 且未拒绝/退回。G1 纯 Approval 分支 result='approved'；
    // G2 ForEach 的 Assignment 分支 result='completed'、EventWait 分支 result IS NULL（COMPLETED/
    // EVENT_RECEIVED 信号语义，见 service::signal_result_str）。rejected/returned 触发 fail-fast
    // 终结整流程（不会走到 join 计数），故此处只排除 rejected/returned 即可覆盖全部分支模板类型。
    let row: (i64,) = dbx
      .fetch_one(
        sqlx::query_as(
          "SELECT COUNT(*) FROM activity_instances WHERE workflow_instance_id = $1 AND status = 'completed' \
           AND (result IS NULL OR result NOT IN ('rejected', 'returned')) \
           AND round = $2 AND activity_definition_id = ANY($3)",
        )
        .bind(instance_id)
        .bind(round)
        .bind(branch_node_ids),
      )
      .await
      .map_err(map_db)?;
    Ok(row.0)
  }

  async fn for_each_branch_count(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    join_node: &str,
    round: i32,
  ) -> Result<Option<i64>> {
    // 取本 round 内指向 join_node 的 FOR_EACH_FANNED_OUT 事件的 branch_count（payload 内 JSON 字段）。
    // ForEach 每 round 扇出一次 → 至多一条；按 seq DESC 取最新兜底。round 以 JSON 数值匹配。
    let row: Option<(i64,)> = dbx
      .fetch_optional(
        sqlx::query_as(
          "SELECT (event_payload->>'branch_count')::bigint FROM workflow_event_log \
           WHERE workflow_instance_id = $1 AND event_type = $2 \
           AND event_payload->>'join_node' = $3 AND (event_payload->>'round')::int = $4 \
           ORDER BY seq DESC LIMIT 1",
        )
        .bind(instance_id)
        .bind(hetuflow_core::event_type::FOR_EACH_FANNED_OUT)
        .bind(join_node)
        .bind(round),
      )
      .await
      .map_err(map_db)?;
    Ok(row.map(|r| r.0))
  }

  async fn load_activity(&self, dbx: &DbxPostgres, activity_id: uuid::Uuid) -> Result<Option<ActivityInstance>> {
    let row = dbx
      .fetch_optional(
        sqlx::query_as::<_, ActivityRow>(&format!("SELECT {ACTIVITY_COLS} FROM activity_instances WHERE id = $1"))
          .bind(activity_id),
      )
      .await
      .map_err(map_db)?;
    Ok(row.map(|r| r.into_model()))
  }

  async fn list_activities(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Vec<ActivityInstance>> {
    let rows = dbx
      .fetch_all(
        sqlx::query_as::<_, ActivityRow>(&format!(
          "SELECT {ACTIVITY_COLS} FROM activity_instances WHERE workflow_instance_id = $1 ORDER BY created_at"
        ))
        .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(rows.into_iter().map(|r| r.into_model()).collect())
  }

  async fn complete_activity_cas(
    &self,
    dbx: &DbxPostgres,
    activity_id: uuid::Uuid,
    result: Option<&str>,
    reviewed_by: i64,
    review_notes: Option<&str>,
  ) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE activity_instances SET status = 'completed', result = $1, reviewed_by = $2, reviewed_at = now(), review_notes = $3, updated_at = now() WHERE id = $4 AND status = 'active'",
        )
        .bind(result)
        .bind(reviewed_by)
        .bind(review_notes)
        .bind(activity_id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn skip_activity_for_timeout(&self, dbx: &DbxPostgres, activity_id: uuid::Uuid) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE activity_instances SET status = 'skipped', updated_at = now() WHERE id = $1 AND status = 'active'",
        )
        .bind(activity_id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn skip_other_active_activities(
    &self,
    dbx: &DbxPostgres,
    instance_id: uuid::Uuid,
    exclude_activity_id: uuid::Uuid,
  ) -> Result<u64> {
    // 先取消这些活动的 pending timers
    dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_timers SET status = 'cancelled' WHERE workflow_instance_id = $1 AND activity_instance_id IN (SELECT id FROM activity_instances WHERE workflow_instance_id = $1 AND status = 'active' AND id != $2) AND status = 'pending'",
        )
        .bind(instance_id)
        .bind(exclude_activity_id),
      )
      .await
      .map_err(map_db)?;
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE activity_instances SET status = 'skipped', updated_at = now() WHERE workflow_instance_id = $1 AND id != $2 AND status = 'active'",
        )
        .bind(instance_id)
        .bind(exclude_activity_id),
      )
      .await
      .map_err(map_db)?;
    Ok(n)
  }

  async fn max_round(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<i32> {
    let row: (i32,) = dbx
      .fetch_one(
        sqlx::query_as("SELECT COALESCE(MAX(round), 0) FROM activity_instances WHERE workflow_instance_id = $1")
          .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(row.0)
  }

  // ============ Event log ============

  async fn next_seq(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<i64> {
    let row: (i64,) = dbx
      .fetch_one(
        sqlx::query_as("SELECT COALESCE(MAX(seq), 0) + 1 FROM workflow_event_log WHERE workflow_instance_id = $1")
          .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(row.0)
  }

  async fn load_events(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid) -> Result<Vec<EventRecord>> {
    #[derive(sqlx::FromRow)]
    struct EventRow {
      seq: i64,
      event_type: String,
      event_payload: serde_json::Value,
      created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = dbx
      .fetch_all(
        sqlx::query_as::<_, EventRow>(
          "SELECT seq, event_type, event_payload, created_at FROM workflow_event_log WHERE workflow_instance_id = $1 ORDER BY seq",
        )
        .bind(instance_id),
      )
      .await
      .map_err(map_db)?;
    Ok(
      rows
        .into_iter()
        .map(|r| EventRecord {
          seq: r.seq,
          event_type: r.event_type,
          payload: r.event_payload,
          created_at: r.created_at,
        })
        .collect(),
    )
  }

  async fn list_instance_ids_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    terminal_only: bool,
    limit: i64,
  ) -> Result<Vec<uuid::Uuid>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT id FROM workflow_instances WHERE true");
    push_scope(&mut qb, scope)?;
    if terminal_only {
      qb.push(" AND status IN ('completed', 'errored', 'cancelled')");
    }
    qb.push(" ORDER BY created_at DESC LIMIT ");
    qb.push_bind(limit);
    let rows: Vec<(uuid::Uuid,)> = dbx.fetch_all(qb.build_query_as::<(uuid::Uuid,)>()).await.map_err(map_db)?;
    Ok(rows.into_iter().map(|r| r.0).collect())
  }

  async fn idempotency_seen(&self, dbx: &DbxPostgres, instance_id: uuid::Uuid, key: &str) -> Result<bool> {
    let row: Option<(bool,)> = dbx
      .fetch_optional(
        sqlx::query_as(
          "SELECT true FROM workflow_event_log WHERE workflow_instance_id = $1 AND idempotency_key = $2 LIMIT 1",
        )
        .bind(instance_id)
        .bind(key),
      )
      .await
      .map_err(map_db)?;
    Ok(row.is_some())
  }

  async fn append_event(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    instance_id: uuid::Uuid,
    seq: i64,
    event_type: &str,
    payload: serde_json::Value,
    idempotency_key: Option<&str>,
  ) -> Result<()> {
    dbx
      .execute(
        sqlx::query(
          "INSERT INTO workflow_event_log (tenant_id, facility_id, workflow_instance_id, seq, event_type, event_payload, idempotency_key)
           VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(tenant_id)
        .bind(facility_id)
        .bind(instance_id)
        .bind(seq)
        .bind(event_type)
        .bind(payload)
        .bind(idempotency_key),
      )
      .await
      .map_err(map_db)?;
    Ok(())
  }

  // ============ Activity Outbox ============

  async fn enqueue_outbox(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
    activity_type: &str,
    payload: &serde_json::Value,
  ) -> Result<()> {
    dbx
      .execute(
        sqlx::query(
          "INSERT INTO workflow_activity_outbox (tenant_id, facility_id, workflow_instance_id, activity_instance_id, activity_type, payload, status, next_attempt_at)
           VALUES ($1, $2, $3, $4, $5, $6, 'pending', now())
           ON CONFLICT DO NOTHING",
        )
        .bind(tenant_id)
        .bind(facility_id)
        .bind(instance_id)
        .bind(activity_id)
        .bind(activity_type)
        .bind(payload),
      )
      .await
      .map_err(map_db)?;
    Ok(())
  }

  async fn poll_due_outbox(
    &self,
    dbx: &DbxPostgres,
    worker_id: &str,
    lease_ttl_secs: i32,
    limit: i64,
  ) -> Result<Vec<OutboxRecord>> {
    let rows = dbx
      .fetch_all(
        sqlx::query_as::<_, OutboxRow>(&format!(
          "UPDATE workflow_activity_outbox
           SET status = 'leased', lease_owner = $1, lease_expires_at = now() + $2 * interval '1 second', updated_at = now()
           WHERE id IN (
             SELECT id FROM workflow_activity_outbox
             WHERE next_attempt_at <= now()
               AND (status = 'pending' OR (status = 'leased' AND lease_expires_at < now()))
             ORDER BY next_attempt_at
             FOR UPDATE SKIP LOCKED
             LIMIT $3
           )
           RETURNING {OUTBOX_COLS}"
        ))
        .bind(worker_id)
        .bind(lease_ttl_secs)
        .bind(limit),
      )
      .await
      .map_err(map_db)?;
    Ok(rows.into_iter().map(|r| r.into_model()).collect())
  }

  async fn mark_outbox_succeeded(&self, dbx: &DbxPostgres, id: uuid::Uuid, worker_id: &str) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_activity_outbox SET status = 'succeeded', lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, updated_at = now()
           WHERE id = $1 AND lease_owner = $2 AND status = 'leased'",
        )
        .bind(id)
        .bind(worker_id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn mark_outbox_retry(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    worker_id: &str,
    backoff_secs: i32,
    last_error: &str,
  ) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_activity_outbox SET status = 'pending', attempt_count = attempt_count + 1, lease_owner = NULL, lease_expires_at = NULL, next_attempt_at = now() + $3 * interval '1 second', last_error = $4, updated_at = now()
           WHERE id = $1 AND lease_owner = $2 AND status = 'leased'",
        )
        .bind(id)
        .bind(worker_id)
        .bind(backoff_secs)
        .bind(last_error),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn mark_outbox_dead_letter(
    &self,
    dbx: &DbxPostgres,
    id: uuid::Uuid,
    worker_id: &str,
    last_error: &str,
  ) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_activity_outbox SET status = 'dead_letter', attempt_count = attempt_count + 1, lease_owner = NULL, lease_expires_at = NULL, last_error = $3, updated_at = now()
           WHERE id = $1 AND lease_owner = $2 AND status = 'leased'",
        )
        .bind(id)
        .bind(worker_id)
        .bind(last_error),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn list_outbox_scoped(
    &self,
    dbx: &DbxPostgres,
    scope: &ScopeFilter,
    instance_id: uuid::Uuid,
  ) -> Result<Vec<OutboxRecord>> {
    let mut qb: QueryBuilder<Postgres> =
      QueryBuilder::new(format!("SELECT {OUTBOX_COLS} FROM workflow_activity_outbox WHERE workflow_instance_id = "));
    qb.push_bind(instance_id);
    push_scope(&mut qb, scope)?;
    qb.push(" ORDER BY created_at");
    let rows = dbx.fetch_all(qb.build_query_as::<OutboxRow>()).await.map_err(map_db)?;
    Ok(rows.into_iter().map(|r| r.into_model()).collect())
  }

  // ============ Timer ============

  async fn schedule_timer(
    &self,
    dbx: &DbxPostgres,
    tenant_id: i64,
    facility_id: uuid::Uuid,
    instance_id: uuid::Uuid,
    activity_id: uuid::Uuid,
    timer_kind: &str,
    fire_in_seconds: i32,
  ) -> Result<()> {
    dbx
      .execute(
        sqlx::query(
          "INSERT INTO workflow_timers (tenant_id, facility_id, workflow_instance_id, activity_instance_id, timer_kind, fire_at, status)
           VALUES ($1, $2, $3, $4, $5, now() + $6 * interval '1 second', 'pending')
           ON CONFLICT (workflow_instance_id, activity_instance_id, timer_kind) WHERE status = 'pending' DO NOTHING",
        )
        .bind(tenant_id)
        .bind(facility_id)
        .bind(instance_id)
        .bind(activity_id)
        .bind(timer_kind)
        .bind(fire_in_seconds),
      )
      .await
      .map_err(map_db)?;
    Ok(())
  }

  async fn cancel_timers_for_activity(&self, dbx: &DbxPostgres, activity_id: uuid::Uuid) -> Result<()> {
    dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_timers SET status = 'cancelled' WHERE activity_instance_id = $1 AND status = 'pending'",
        )
        .bind(activity_id),
      )
      .await
      .map_err(map_db)?;
    Ok(())
  }

  async fn poll_due_timers(&self, dbx: &DbxPostgres, limit: i64) -> Result<Vec<TimerRecord>> {
    let rows = dbx
      .fetch_all(
        sqlx::query_as::<_, TimerRow>(
          "SELECT id, tenant_id, facility_id, workflow_instance_id, activity_instance_id, timer_kind
           FROM workflow_timers
           WHERE status = 'pending' AND fire_at <= now()
           ORDER BY fire_at
           FOR UPDATE SKIP LOCKED
           LIMIT $1",
        )
        .bind(limit),
      )
      .await
      .map_err(map_db)?;
    Ok(rows.into_iter().map(|r| r.into_record()).collect())
  }

  async fn update_activity_assignee_role(
    &self,
    dbx: &DbxPostgres,
    activity_id: uuid::Uuid,
    new_role: &str,
  ) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE activity_instances SET assignee_role = $1, updated_at = now() WHERE id = $2 AND status = 'active'",
        )
        .bind(new_role)
        .bind(activity_id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }

  async fn mark_timer_fired(&self, dbx: &DbxPostgres, id: uuid::Uuid) -> Result<bool> {
    let n = dbx
      .execute(
        sqlx::query(
          "UPDATE workflow_timers SET status = 'fired', fired_at = now() WHERE id = $1 AND status = 'pending'",
        )
        .bind(id),
      )
      .await
      .map_err(map_db)?;
    Ok(n > 0)
  }
}
