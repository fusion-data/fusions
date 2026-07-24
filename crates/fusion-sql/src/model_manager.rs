use std::future::Future;

use chrono::{DateTime, FixedOffset};
use fusion_common::ctx::Ctx;

use crate::config::DbConfig;
use crate::id::Id;
use crate::store::dbx::DbxProviderTrait;
use crate::store::{Dbx, create_dbx};
use crate::{Result, SqlError};

pub trait ModelContext: Clone + Send + Sync + 'static {
  fn audit_user_id(&self) -> Id;

  fn req_time(&self) -> DateTime<FixedOffset>;

  fn db_session_vars(&self) -> Vec<(&'static str, String)> {
    Vec::new()
  }
}

impl ModelContext for Ctx {
  /// # ⚠️ 0 哨兵语义
  ///
  /// `Ctx` 没有 user id 时返回 `0`：`created_by` / `updated_by` 审计列会以
  /// user 0 落库，表示「system / 未归因」写入。依赖精确归因的应用 MUST 使用
  /// 自定义 `AppContext: ModelContext`（把 audit actor 设为必填字段），而不是
  /// 依赖本兼容 impl；读侧可用 `created_by = 0` 识别未归因写入。
  fn audit_user_id(&self) -> Id {
    match self.get_user_id() {
      Some(user_id) => user_id.into(),
      None => {
        log::debug!("Ctx has no user id; audit columns fall back to sentinel user 0");
        Id::I64(0)
      }
    }
  }

  fn req_time(&self) -> DateTime<FixedOffset> {
    Ctx::req_time(self).to_owned()
  }
}

pub type DefaultModelManager = ModelManager<Ctx>;

#[derive(Clone)]
pub struct ModelManager<C: ModelContext = Ctx> {
  dbx: Dbx,
  ctx: Option<C>,
}

/// 手写 Debug：让下游 `#[derive(Debug)]` 包装 `ModelManager` 的常见写法可编译。
/// `C: ModelContext` 不要求 `Debug`（且 ctx 可能含敏感上下文），只打印类型名；
/// dbx 只打印 provider 与事务标记，避免连接细节入日志。
impl<C: ModelContext> std::fmt::Debug for ModelManager<C> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ModelManager")
      .field("dbx", &self.dbx.provider())
      .field("is_txn", &self.dbx.is_txn())
      .field("ctx", &self.ctx.as_ref().map(|_| std::any::type_name::<C>()))
      .finish()
  }
}

impl<C> ModelManager<C>
where
  C: ModelContext,
{
  /// Constructor
  pub async fn new(db_config: &DbConfig, application_name: Option<&str>) -> Result<Self> {
    let dbx = create_dbx(db_config, application_name).await?;
    Ok(Self { dbx, ctx: None })
  }

  pub fn new_with_dbx(dbx: Dbx) -> Self {
    Self { dbx, ctx: None }
  }

  /// 强制返回一个新的事务，即使当前 ModelManager 已开启事务。
  pub fn txn_cloned(&self) -> ModelManager<C> {
    let dbx = self.dbx.txn_cloned();
    ModelManager { dbx, ctx: self.ctx.clone() }
  }

  /// 若当前 ModelManager 已开启事务，则返回self的克隆，否则返回一个新的事务。
  pub fn get_txn_clone(&self) -> ModelManager<C> {
    if self.dbx().is_txn() { self.clone() } else { self.txn_cloned() }
  }

  pub fn dbx(&self) -> &Dbx {
    &self.dbx
  }

  pub fn ctx_ref(&self) -> Result<&C> {
    self.ctx.as_ref().ok_or(SqlError::CtxMissing)
  }

  pub fn ctx_opt_ref(&self) -> Option<&C> {
    self.ctx.as_ref()
  }

  pub fn with_ctx(mut self, ctx: C) -> Self {
    self.dbx = self.dbx.with_session_vars(ctx.db_session_vars());
    self.ctx = Some(ctx);
    self
  }

  /// 闭包式事务 API
  ///
  /// 自动管理事务的生命周期，在闭包执行成功时提交事务，失败时回滚事务。
  ///
  /// # ⚠️ Warning — RLS / session vars 不会被注入
  ///
  /// 此方法**不会**调用 `SET LOCAL app.tenant_id = ...` 之类 session vars 注入；
  /// 它只是 `BEGIN; ...; COMMIT;` 的封装。如果你的应用依赖 PostgreSQL Row Level
  /// Security（FORCE ROW LEVEL SECURITY 表 + `app.*` GUC 谓词），必须使用配套
  /// 的应用层 helper（如 hylx-careos 项目的 `hylx_core::db::with_read_txn` /
  /// `with_write_txn`），它们在 `transaction` 之上叠加 `set_config(...)`。
  /// 直接调用此方法跑 RLS 表 SQL 会按 fallback policy 处理 → 跨租户读放大风险。
  ///
  /// # 嵌套事务支持
  ///
  /// 该方法支持嵌套调用，内部使用 SAVEPOINT 机制。
  pub async fn transaction<F, Fut, T>(&self, f: F) -> Result<T>
  where
    F: FnOnce(ModelManager<C>) -> Fut,
    Fut: Future<Output = Result<T>>,
  {
    self.run_in_txn(false, f).await
  }

  /// 闭包式只读事务 API
  ///
  /// 与 [`Self::transaction`] 类似，但顶层会发 `SET TRANSACTION READ ONLY`，
  /// 闭包内任何 INSERT/UPDATE/DELETE 都会被 PostgreSQL 拒绝，提供"读路径不写"
  /// 的硬保障。`set_config(...)` 仍可正常注入 RLS session vars。
  ///
  /// 嵌套场景（外层已是事务）下，仅起 SAVEPOINT；savepoint 继承外层事务的
  /// 读写模式，`read_transaction` 内嵌套不会把外层写事务降级为只读。
  pub async fn read_transaction<F, Fut, T>(&self, f: F) -> Result<T>
  where
    F: FnOnce(ModelManager<C>) -> Fut,
    Fut: Future<Output = Result<T>>,
  {
    self.run_in_txn(true, f).await
  }

  async fn run_in_txn<F, Fut, T>(&self, read_only: bool, f: F) -> Result<T>
  where
    F: FnOnce(ModelManager<C>) -> Fut,
    Fut: Future<Output = Result<T>>,
  {
    let mm_txn = self.get_txn_clone();
    if read_only {
      mm_txn.dbx().begin_txn_read_only().await?;
    } else {
      mm_txn.dbx().begin_txn().await?;
    }

    match f(mm_txn.clone()).await {
      Ok(result) => {
        mm_txn.dbx().commit_txn().await?;
        Ok(result)
      }
      Err(e) => {
        if let Err(rollback_err) = mm_txn.dbx().rollback_txn().await {
          log::warn!("Failed to rollback transaction: {:?}", rollback_err);
        }
        Err(e)
      }
    }
  }
}
