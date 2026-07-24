//! Adapter-owned ports. The framework decides *what* must happen; these ports decide *how* the
//! consuming application does it (transaction boundaries + session context, notification delivery,
//! business callback routing).
//!
//! Framework crates MUST NOT know the application's auth, scope, tenancy GUCs, notification
//! provider or business handlers (`hetuflow.md` §3 / §10) — every such concern arrives here.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use fusion_sql::store::DbxPostgres;
use hetuflow_core::{BusinessCallbackHandler, CallbackFuture, FlowError, NotificationPayload, Result};

/// Unit-of-work port: hands the framework a `&DbxPostgres` that is **already inside a write
/// transaction with the application's session context injected** (RLS GUCs, audit context …),
/// commits on `Ok`, rolls back on `Err`.
///
/// This is the A1 invariant of `hetuflow-sqlx` lifted to the orchestration layer: the framework
/// never opens a transaction and never sets a session variable. Only the async workers need it —
/// request-driven paths (`WorkflowService::start` / `signal` / …) take the caller's `&DbxPostgres`
/// directly, because the adapter's RPC handler already owns that transaction.
///
/// Implementations MUST propagate the closure's [`FlowError`] unchanged (an implementation that
/// stringifies it through its own error type destroys the not-found / conflict / validation
/// distinction the RPC layer maps to status codes).
pub trait TxnRunner: Send + Sync + 'static {
  /// Cross-tenant maintenance transaction — used by the pollers, whose queries deliberately scan
  /// every tenant's due rows. The application MUST grant this context whatever row visibility its
  /// isolation model requires (e.g. a system-maintenance scope for FORCE-RLS tables).
  fn system_write<'a, F, Fut, T>(&'a self, reason: &'a str, f: F) -> TxnFuture<'a, T>
  where
    F: FnOnce(DbxPostgres) -> Fut + Send + 'a,
    Fut: Future<Output = Result<T>> + Send + 'a,
    T: Send + 'a;

  /// Tenant-fenced transaction — used once a polled row's owning tenant is known, so the follow-up
  /// writes (and any workflow advance they trigger) run under normal tenant isolation.
  fn tenant_write<'a, F, Fut, T>(&'a self, tenant_id: i64, reason: &'a str, f: F) -> TxnFuture<'a, T>
  where
    F: FnOnce(DbxPostgres) -> Fut + Send + 'a,
    Fut: Future<Output = Result<T>> + Send + 'a,
    T: Send + 'a;
}

/// Boxed on purpose rather than RPITIT: an `impl Future + Send + 'a` return that captures the
/// `&'a self` lifetime cannot be proven `Send` "generally enough" when the worker loop is handed
/// to `tokio::spawn` (rust-lang/rust#100013). A boxed future erases that lifetime dance.
pub type TxnFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Notification delivery port. `template_code` / `recipient_selector` / `channel_policy` are
/// opaque to the framework — the adapter owns provider routing, templating and channel policy.
pub trait NotificationDispatcher: Send + Sync + 'static {
  fn deliver<'a>(&'a self, payload: &'a NotificationPayload) -> CallbackFuture<'a>;
}

/// Business callback routing table: `handler_type` → handler. Registration is fail-closed
/// (duplicate `handler_type` is refused at build time, not silently last-wins), and
/// [`WorkflowService::start`](crate::WorkflowService::start) refuses a terminal callback whose
/// handler is not registered — so a workflow can never reach its terminal state and *then*
/// discover there is nobody to run its side effect.
#[derive(Default, Clone)]
pub struct CallbackRegistry {
  handlers: BTreeMap<String, Arc<dyn BusinessCallbackHandler>>,
}

impl std::fmt::Debug for CallbackRegistry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CallbackRegistry").field("handler_types", &self.handler_types()).finish()
  }
}

impl CallbackRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Register a handler under its own `handler_type`. Duplicate registration is an error.
  pub fn register(&mut self, handler: Arc<dyn BusinessCallbackHandler>) -> Result<()> {
    let key = handler.handler_type().to_string();
    if key.is_empty() {
      return Err(FlowError::Validation("business callback handler_type must not be empty".into()));
    }
    if self.handlers.contains_key(&key) {
      return Err(FlowError::Conflict(format!("business callback handler '{key}' already registered")));
    }
    self.handlers.insert(key, handler);
    Ok(())
  }

  pub fn get(&self, handler_type: &str) -> Option<&Arc<dyn BusinessCallbackHandler>> {
    self.handlers.get(handler_type)
  }

  pub fn contains(&self, handler_type: &str) -> bool {
    self.handlers.contains_key(handler_type)
  }

  pub fn handler_types(&self) -> Vec<&str> {
    self.handlers.keys().map(String::as_str).collect()
  }

  pub fn is_empty(&self) -> bool {
    self.handlers.is_empty()
  }
}
