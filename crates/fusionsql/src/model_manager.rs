use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use fusion_common::ctx::Ctx;
use fusionsql_core::filter::FilterGroups;

use crate::base::BmcConfig;
use crate::config::DbConfig;
use crate::id::Id;
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
  fn audit_user_id(&self) -> Id {
    self.get_user_id().unwrap_or(0).into()
  }

  fn req_time(&self) -> DateTime<FixedOffset> {
    Ctx::req_time(self).to_owned()
  }
}

pub type DefaultModelManager = ModelManager<Ctx>;

/// 过滤器拦截器类型
pub type FilterInterceptor<C> = Arc<dyn Fn(&BmcConfig, Option<&C>, FilterGroups) -> Result<FilterGroups> + Send + Sync>;

#[derive(Clone)]
pub struct ModelManager<C: ModelContext = Ctx> {
  dbx: Dbx,
  ctx: Option<C>,
  filter_interceptor: Option<FilterInterceptor<C>>,
}

impl<C> ModelManager<C>
where
  C: ModelContext,
{
  /// Constructor
  pub async fn new(db_config: &DbConfig, application_name: Option<&str>) -> Result<Self> {
    let dbx = create_dbx(db_config, application_name).await?;
    Ok(Self { dbx, ctx: None, filter_interceptor: None })
  }

  pub fn new_with_dbx(dbx: Dbx) -> Self {
    Self { dbx, ctx: None, filter_interceptor: None }
  }

  /// 强制返回一个新的事务，即使当前 ModelManager 已开启事务。
  pub fn txn_cloned(&self) -> ModelManager<C> {
    let dbx = self.dbx.txn_cloned();
    ModelManager { dbx, ctx: self.ctx.clone(), filter_interceptor: self.filter_interceptor.clone() }
  }

  /// 若当前 ModelManager 已开启事务，则返回self的克隆，否则返回一个新的事务。
  pub fn get_txn_clone(&self) -> ModelManager<C> {
    if self.dbx().is_txn() { self.clone() } else { self.txn_cloned() }
  }

  pub fn dbx(&self) -> &Dbx {
    &self.dbx
  }

  pub fn ctx_ref(&self) -> Result<&C> {
    self.ctx.as_ref().ok_or(SqlError::Unauthorized("The ctx of ModelManager is not set".to_string()))
  }

  pub fn ctx_opt_ref(&self) -> Option<&C> {
    self.ctx.as_ref()
  }

  pub fn with_ctx(mut self, ctx: C) -> Self {
    self.dbx = self.dbx.with_session_vars(ctx.db_session_vars());
    self.ctx = Some(ctx);
    self
  }

  /// 设置过滤器拦截器
  /// 拦截器函数会在查询执行前被调用，可以修改过滤条件
  pub fn with_filter_interceptor<F>(mut self, interceptor: F) -> Self
  where
    F: Fn(&BmcConfig, Option<&C>, FilterGroups) -> Result<FilterGroups> + Send + Sync + 'static,
  {
    self.filter_interceptor = Some(Arc::new(interceptor));
    self
  }

  /// 应用过滤器拦截器（如果存在）
  pub fn apply_filter_interceptor(&self, bmc_config: &BmcConfig, filters: FilterGroups) -> Result<FilterGroups> {
    if let Some(interceptor) = self.filter_interceptor.as_ref() {
      interceptor(bmc_config, self.ctx_opt_ref(), filters)
    } else {
      Ok(filters)
    }
  }

  /// 应用过滤器拦截器（可选CTX版本）
  pub fn apply_filter_interceptor_with_ctx(
    &self,
    bmc_config: &BmcConfig,
    ctx: &C,
    filters: FilterGroups,
  ) -> Result<FilterGroups> {
    if let Some(interceptor) = &self.filter_interceptor {
      interceptor(bmc_config, Some(ctx), filters)
    } else {
      Ok(filters)
    }
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
  /// # 示例
  ///
  /// ```ignore
  /// use fusionsql::ModelManager;
  ///
  /// async fn example(mm: &ModelManager<MyCtx>) -> Result<()> {
  ///   mm.transaction(|mm| async move {
  ///     // 在事务中执行多个操作
  ///     UserBmc::create(&mm, user).await?;
  ///     UserBmc::update(&mm, id, update).await?;
  ///     Ok(())
  ///   }).await?;
  ///   Ok(())
  /// }
  /// ```
  ///
  /// # 嵌套事务支持
  ///
  /// 该方法支持嵌套调用，内部使用 SAVEPOINT 机制：
  ///
  /// ```ignore
  /// async fn nested_example(mm: &ModelManager<MyCtx>) -> Result<()> {
  ///   mm.transaction(|mm| async move {
  ///     UserBmc::create(&mm, user1).await?;
  ///
  ///     // 嵌套事务
  ///     mm.transaction(|mm| async move {
  ///       UserBmc::create(&mm, user2).await?;
  ///       Ok(())
  ///     }).await?;
  ///
  ///     Ok(())
  ///   }).await?;
  ///   Ok(())
  /// }
  /// ```
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
