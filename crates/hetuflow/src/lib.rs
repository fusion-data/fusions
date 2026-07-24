//! hetuflow —— 工作流框架聚合 crate。
//!
//! 业务系统推荐依赖本 crate（而非直接依赖 `hetuflow-core/runtime/sqlx`），通过 features
//! 选择性引入子能力并重导出：
//! - `core`：领域类型 + 纯 helper
//! - `runtime`：图校验 + 推进决策（含 core）
//! - `sqlx`：Postgres 存储（含 runtime；接 caller 的 `DbxPostgres` 事务）
//! - `service`：事务内编排 + outbox / timer worker 骨架（含 sqlx）
//!
//! 宿主绑定层用 `features = ["service"]`，经 adapter 做 proto↔core 映射 + scope/审计/通知绑定；
//! 只做设计态校验的消费者（如 IR 编译器）用默认 `runtime`，其依赖闭包内无 store、无编排。

#[cfg(feature = "core")]
pub use hetuflow_core as core;

#[cfg(feature = "runtime")]
pub use hetuflow_runtime as runtime;

#[cfg(feature = "service")]
pub use hetuflow_service as service;

#[cfg(feature = "sqlx")]
pub use hetuflow_sqlx as sqlx;

pub mod prelude {
  #[cfg(feature = "core")]
  pub use hetuflow_core::{
    ActivityInstance, ActivityStatus, ActivityType, AdvanceOutcome, DefinitionSnapshot, FacilityVisibility, FlowError,
    NodeConfig, NodeKind, NotificationPayload, OutboxRecord, Result, ScopeFilter, SignalType, TimerRecord,
    WorkflowDefinition, WorkflowInstance, WorkflowNode, WorkflowResult, WorkflowStatus, WorkflowTransition, event_type,
    outbox_activity,
  };
  #[cfg(feature = "runtime")]
  pub use hetuflow_runtime::{decide_advance, decide_start, find_next_transition, validate_definition};
  /// 宿主绑定层的编排入口 + 端口 + worker 骨架。
  #[cfg(feature = "service")]
  pub use hetuflow_service::{
    CallbackRegistry, NotificationDispatcher, OutboxDispatcher, SignalCommand, SignalOutcome, StartCommand,
    StartOutcome, TerminalCallback, TimerPoller, TxnFuture, TxnRunner, WorkerConfig, WorkflowConfig, WorkflowService,
  };
  /// Phase H-c：业务方推荐通过聚合 prelude 引入 store handle + trait（PG-only）。
  #[cfg(feature = "sqlx")]
  pub use hetuflow_sqlx::{PgWorkflowStore, WorkflowStore};
}
