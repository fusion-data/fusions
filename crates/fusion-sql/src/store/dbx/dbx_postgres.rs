use std::net::ToSocketAddrs;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use log::{debug, info, warn};
use sqlx::Executor;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, Postgres};
use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::{ConnectOptions, FromRow, IntoArguments, Pool, Transaction};
use tokio::sync::Mutex;

use crate::DbConfig;

use super::{DbxError, Result};

pub type Db = Pool<Postgres>;

pub async fn new_pg_pool_from_config(c: &DbConfig, application_name: Option<&str>) -> Result<Db> {
  if !c.enable() {
    return Err(DbxError::ConfigInvalid("Need set fusion.db.enable = true"));
  }

  let mut pool = PgPoolOptions::new();
  if let Some(v) = c.max_connections() {
    pool = pool.max_connections(v);
  }
  if let Some(v) = c.min_connections() {
    pool = pool.min_connections(v);
  }
  if let Some(v) = c.acquire_timeout() {
    pool = pool.acquire_timeout(*v);
  }
  if let Some(v) = c.idle_timeout() {
    pool = pool.idle_timeout(*v);
  }
  if let Some(v) = c.max_lifetime() {
    pool = pool.max_lifetime(*v);
  }
  if let Some(v) = c.after_connect() {
    let query = v.to_string();

    pool = pool.after_connect(move |conn, _| {
      let query = query.clone();
      Box::pin(async move {
        conn.execute(query.as_str()).await?;
        Ok(())
      })
    });
  }

  let level = log::LevelFilter::Debug;
  let mut opts: PgConnectOptions = match c.url() {
    Some(url) => url.parse()?,
    None => {
      let mut o = PgConnectOptions::new();
      if let Some(host) = c.host() {
        o = o.host(host);
      }
      if let Some(port) = c.port() {
        o = o.port(port);
      }
      if let Some(socket) = c.socket() {
        o = o.socket(socket);
      }
      if let Some(database) = c.database() {
        o = o.database(database);
      }
      if let Some(username) = c.username() {
        o = o.username(username);
      }
      if let Some(password) = c.password() {
        o = o.password(password);
      }
      o
    }
  };
  if let Some(an) = c.application_name().or(application_name) {
    opts = opts.application_name(an);
  }
  if let Some(search_path) = c.schema_search_path() {
    opts = opts.options([("search_path", search_path)]);
  }

  // 若 opts.host 是域名，需要进行DNS查找将期转换为 ip addr
  let non_ip_addr = opts.get_host() != "localhost" && opts.get_host().parse::<std::net::IpAddr>().is_err();
  if non_ip_addr {
    let original_host = format!("{}:{}", opts.get_host(), opts.get_port());
    // DNS 解析失败 / 解析结果为空时不再 panic 整个进程，改为返回配置类错误。
    let sock_addr = original_host
      .to_socket_addrs()
      .map_err(|_| DbxError::ConfigInvalid("Failed to resolve database host (DNS lookup failed)"))?
      .next()
      .ok_or(DbxError::ConfigInvalid("DNS resolved to empty address set"))?;
    opts = opts.host(&sock_addr.ip().to_string());
    debug!("Resolve original host, from {} to {}", original_host, opts.get_host());
  }

  opts = opts.log_statements(level);
  let log_opts = opts.clone().password("<password>");
  info!("Postgres connect options: {:?}", log_opts);

  let db = pool.connect_with(opts).await?;
  info!("Postgres pool connected (size={})", db.size());
  Ok(db)
}

/// PostgreSQL 连接句柄，封装连接池与（可选的）事务持有者。
///
/// **并发约束**：处于事务态的 `DbxPostgres`（及其上层 `ModelManager`）**不可
/// 跨 tokio task 并发使用**。`txn_holder` 内持有的 `Transaction` 绑定到单条
/// 物理连接，begin/commit 计数器假设串行执行；把同一个事务态句柄 `clone` 后
/// 在多个并发 task 中混用会导致事务深度计数错乱、SQL 交错以及 RLS GUC 失效。
/// 每个并发分支应各自走独立的 `with_read_txn` / `with_write_txn`。
#[derive(Debug, Clone)]
pub struct DbxPostgres {
  db_pool: Db,
  txn_holder: Arc<Mutex<Option<TxnHolder>>>,
  txn: bool,
  session_vars: Arc<Vec<(String, String)>>,
}

impl DbxPostgres {
  pub fn new(db_pool: Db, txn: bool) -> Self {
    DbxPostgres { db_pool, txn_holder: Arc::default(), txn, session_vars: Arc::default() }
  }

  pub fn is_txn(&self) -> bool {
    self.txn
  }

  pub fn non_txn(&self) -> bool {
    !self.txn
  }

  pub fn txn_cloned(&self) -> Self {
    Self {
      db_pool: self.db_pool.clone(),
      txn_holder: Arc::default(),
      txn: true,
      session_vars: self.session_vars.clone(),
    }
  }

  /// Defense-in-depth assert: if `session_vars` is non-empty (caller intends to
  /// run under RLS GUC) but we're about to fall through to a non-txn pool
  /// query (where `SET LOCAL` cannot apply), that's a wiring bug — the caller
  /// likely forgot to wrap the query in `with_read_txn` / `with_write_txn`.
  /// Catches RLS-bypass mistakes early in dev/test (release builds skip).
  fn assert_no_orphan_session_vars(&self) {
    debug_assert!(
      self.txn || self.session_vars.is_empty(),
      "DbxPostgres: session_vars set but txn=false → SET LOCAL won't apply, RLS GUC will be empty (RLS-bypass risk). \
       Wrap the call in your application's `with_read_txn` / `with_write_txn` helper (e.g. `<app>::db::with_read_txn`)."
    );
  }

  pub fn with_session_vars(&self, vars: Vec<(&'static str, String)>) -> Self {
    Self {
      db_pool: self.db_pool.clone(),
      txn_holder: self.txn_holder.clone(),
      txn: self.txn,
      session_vars: Arc::new(vars.into_iter().map(|(key, value)| (key.to_string(), value)).collect()),
    }
  }
}

impl DbxPostgres {
  pub async fn begin_txn(&self) -> Result<()> {
    self.begin_txn_inner(false).await
  }

  /// 启动一个只读事务（顶层）或在已有事务上加 SAVEPOINT。
  ///
  /// 顶层场景下，`pool.begin()` 后立即发 `SET TRANSACTION READ ONLY`，
  /// 之后再发 `set_config(..., is_local=true)` 注入 RLS session vars——
  /// `set_config` 不被 READ ONLY 拒绝，整个事务内 INSERT/UPDATE/DELETE
  /// 会被 PG 拒绝，提供"读路径绝不写"的硬保障。
  ///
  /// 嵌套场景（已有 outer txn）下，仅创建 SAVEPOINT；savepoint 继承
  /// 外层事务的读写模式，无法把已存在的写事务"降级"为只读。
  pub async fn begin_txn_read_only(&self) -> Result<()> {
    self.begin_txn_inner(true).await
  }

  async fn begin_txn_inner(&self, read_only: bool) -> Result<()> {
    if !self.txn {
      return Err(DbxError::CannotBeginTxnWithTxnFalse);
    }

    let mut txh_g = self.txn_holder.lock().await;
    // If we already have a tx holder, then, we create a savepoint
    if let Some(txh) = txh_g.as_mut() {
      let savepoint_name = txh.inc();
      let sql = format!("SAVEPOINT {}", savepoint_name);
      sqlx::query(&sql)
        .execute(txh.txn.as_mut())
        .await
        .map_err(|e| DbxError::SavePointError(format!("Failed to create savepoint '{}': {}", savepoint_name, e)))?;
    } else {
      // If not, we create one with a new transaction
      let txn = self.db_pool.begin().await?;
      let mut txh = TxnHolder::new(txn);
      if read_only {
        sqlx::query("SET TRANSACTION READ ONLY").execute(txh.txn.as_mut()).await?;
      }
      if !self.session_vars.is_empty() {
        let expressions = (0..self.session_vars.len())
          .map(|idx| format!("set_config(${}, ${}, true)", idx * 2 + 1, idx * 2 + 2))
          .collect::<Vec<_>>()
          .join(", ");
        let sql = format!("SELECT {expressions}");
        let mut query = sqlx::query(&sql);
        for (key, value) in self.session_vars.iter() {
          query = query.bind(key).bind(value);
        }
        debug!(
          "DbxPostgres.begin_txn applying session_vars (read_only={}): {}",
          read_only,
          self.session_vars.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
        );
        query.execute(txh.txn.as_mut()).await?;
      }
      let _ = txh_g.insert(txh);
    }

    Ok(())
  }

  pub async fn rollback_txn(&self) -> Result<()> {
    let mut txh_g = self.txn_holder.lock().await;
    if let Some(txh) = txh_g.as_mut() {
      let (counter, savepoint) = txh.dec();

      if counter == 0 {
        // 回滚整个事务
        if let Some(txh) = txh_g.take() {
          txh.txn.rollback().await?;
          debug!("DbxPostgres.rollback_txn: transaction rolled back");
        }
      } else if let Some(sp) = savepoint {
        // 回滚到 SAVEPOINT
        let sql = format!("ROLLBACK TO SAVEPOINT {}", sp);
        sqlx::query(&sql)
          .execute(txh.txn.as_mut())
          .await
          .map_err(|e| DbxError::SavePointError(format!("Failed to rollback to savepoint '{}': {}", sp, e)))?;
        debug!("DbxPostgres.rollback_txn: rolled back to savepoint '{}', transaction depth now {}", sp, counter);
      } else {
        // counter > 0 但没有 savepoint，理论上不应该发生
        warn!("DbxPostgres.rollback_txn: nested rollback with depth {} but no savepoint to rollback to", counter);
      }

      Ok(())
    } else {
      Err(DbxError::NoTxn)
    }
  }

  pub async fn commit_txn(&self) -> Result<()> {
    if !self.txn {
      return Err(DbxError::CannotCommitTxnWithTxnFalse);
    }

    let mut txh_g = self.txn_holder.lock().await;
    if let Some(txh) = txh_g.as_mut() {
      let (counter, savepoint) = txh.dec();
      // If 0, then, it should be matching commit for the first begin_txn
      // so we can commit.
      if counter == 0 {
        // here we take the txh out of the option
        if let Some(txh) = txh_g.take() {
          txh.txn.commit().await?;
        } else {
          // 计数为 0 但持有者为空，理论上不可能发生，记录警告以便排查
          warn!(
            "DbxPostgres.commit_txn: counter reached 0 but txn_holder was None; possible logic error, commit skipped"
          );
        }
      } else if let Some(sp) = savepoint {
        // 嵌套事务场景，释放 SAVEPOINT
        let sql = format!("RELEASE SAVEPOINT {}", sp);
        sqlx::query(&sql)
          .execute(txh.txn.as_mut())
          .await
          .map_err(|e| DbxError::SavePointError(format!("Failed to release savepoint '{}': {}", sp, e)))?;
        debug!("DbxPostgres.commit_txn: nested commit released savepoint '{}', transaction depth now {}", sp, counter);
      } else {
        // counter > 0 但没有 savepoint，理论上不应该发生
        warn!("DbxPostgres.commit_txn: nested commit with depth {} but no savepoint to release", counter);
      }

      Ok(())
    } else {
      // Otherwise, we have an error
      Err(DbxError::TxnCantCommitNoOpenTxn)
    }
  }

  pub fn db(&self) -> &Db {
    &self.db_pool
  }

  pub async fn fetch_one<'q, O, A>(&self, query: QueryAs<'q, Postgres, O, A>) -> Result<O>
  where
    O: for<'r> FromRow<'r, <Postgres as sqlx::Database>::Row> + Send + Unpin,
    A: IntoArguments<'q, Postgres> + 'q,
  {
    if self.txn {
      let mut txh_g = self.txn_holder.lock().await;
      if let Some(txn) = txh_g.as_deref_mut() {
        let res = query.fetch_one(txn.as_mut()).await?;
        return Ok(res);
      }
    }

    self.assert_no_orphan_session_vars();
    let res = query.fetch_one(self.db()).await?;
    Ok(res)
  }

  pub async fn fetch_optional<'q, O, A>(&self, query: QueryAs<'q, Postgres, O, A>) -> Result<Option<O>>
  where
    O: for<'r> FromRow<'r, <Postgres as sqlx::Database>::Row> + Send + Unpin,
    A: IntoArguments<'q, Postgres> + 'q,
  {
    let data = if self.txn {
      let mut txh_g = self.txn_holder.lock().await;
      if let Some(txn) = txh_g.as_deref_mut() {
        query.fetch_optional(txn.as_mut()).await?
      } else {
        self.assert_no_orphan_session_vars();
        query.fetch_optional(self.db()).await?
      }
    } else {
      self.assert_no_orphan_session_vars();
      query.fetch_optional(self.db()).await?
    };

    Ok(data)
  }

  pub async fn fetch_all<'q, O, A>(&self, query: QueryAs<'q, Postgres, O, A>) -> Result<Vec<O>>
  where
    O: for<'r> FromRow<'r, <Postgres as sqlx::Database>::Row> + Send + Unpin,
    A: IntoArguments<'q, Postgres> + 'q,
  {
    let data = if self.txn {
      let mut txh_g = self.txn_holder.lock().await;
      if let Some(txn) = txh_g.as_deref_mut() {
        query.fetch_all(txn.as_mut()).await?
      } else {
        self.assert_no_orphan_session_vars();
        query.fetch_all(self.db()).await?
      }
    } else {
      self.assert_no_orphan_session_vars();
      query.fetch_all(self.db()).await?
    };

    Ok(data)
  }

  pub async fn execute<'q, A>(&self, query: Query<'q, Postgres, A>) -> Result<u64>
  where
    A: IntoArguments<'q, Postgres> + 'q,
  {
    let row_affected = if self.txn {
      let mut txh_g = self.txn_holder.lock().await;
      if let Some(txn) = txh_g.as_deref_mut() {
        query.execute(txn.as_mut()).await?.rows_affected()
      } else {
        self.assert_no_orphan_session_vars();
        query.execute(self.db()).await?.rows_affected()
      }
    } else {
      self.assert_no_orphan_session_vars();
      query.execute(self.db()).await?.rows_affected()
    };

    Ok(row_affected)
  }

  pub async fn fetch_one_scalar<'q, O, A>(&self, query: QueryScalar<'q, Postgres, O, A>) -> Result<O>
  where
    O: sqlx::Decode<'q, Postgres> + sqlx::Type<Postgres> + Send + Unpin,
    A: 'q + IntoArguments<'q, Postgres> + Send,
    (O,): for<'r> FromRow<'r, <Postgres as sqlx::Database>::Row>,
  {
    let data = if self.txn {
      let mut txh_g = self.txn_holder.lock().await;
      if let Some(txn) = txh_g.as_deref_mut() {
        query.fetch_one(txn.as_mut()).await?
      } else {
        self.assert_no_orphan_session_vars();
        query.fetch_one(self.db()).await?
      }
    } else {
      self.assert_no_orphan_session_vars();
      query.fetch_one(self.db()).await?
    };
    Ok(data)
  }

  pub async fn fetch_optional_scalar<'q, O, A>(&self, query: QueryScalar<'q, Postgres, O, A>) -> Result<Option<O>>
  where
    O: sqlx::Decode<'q, Postgres> + sqlx::Type<Postgres> + Send + Unpin,
    A: 'q + IntoArguments<'q, Postgres> + Send,
    (O,): for<'r> FromRow<'r, <Postgres as sqlx::Database>::Row>,
  {
    let data = if self.txn {
      let mut txh_g = self.txn_holder.lock().await;
      if let Some(txn) = txh_g.as_deref_mut() {
        query.fetch_optional(txn.as_mut()).await?
      } else {
        self.assert_no_orphan_session_vars();
        query.fetch_optional(self.db()).await?
      }
    } else {
      self.assert_no_orphan_session_vars();
      query.fetch_optional(self.db()).await?
    };
    Ok(data)
  }

  pub async fn fetch_all_scalar<'q, O, A>(&self, query: QueryScalar<'q, Postgres, O, A>) -> Result<Vec<O>>
  where
    O: sqlx::Decode<'q, Postgres> + sqlx::Type<Postgres> + Send + Unpin,
    A: 'q + IntoArguments<'q, Postgres> + Send,
    (O,): for<'r> FromRow<'r, <Postgres as sqlx::Database>::Row>,
  {
    let data = if self.txn {
      let mut txh_g = self.txn_holder.lock().await;
      if let Some(txn) = txh_g.as_deref_mut() {
        query.fetch_all(txn.as_mut()).await?
      } else {
        self.assert_no_orphan_session_vars();
        query.fetch_all(self.db()).await?
      }
    } else {
      self.assert_no_orphan_session_vars();
      query.fetch_all(self.db()).await?
    };
    Ok(data)
  }
}

#[derive(Debug)]
struct TxnHolder {
  txn: Transaction<'static, Postgres>,
  counter: i32,
  savepoints: Vec<String>,
  /// 创建此事务句柄的 tokio task id（仅 debug 期护栏）。
  ///
  /// `DbxPostgres` 是 `Clone` 且 `txn_holder` 在克隆体间共享 `Arc`；事务态句柄
  /// 被并发用在两个 tokio task 时，`inc`/`dec` 的 savepoint 计数与栈会交错错乱。
  /// 在每个事务态操作入口断言「当前 task id == owner task id」可在 debug build
  /// 直接 panic 暴露此误用。`tokio::task::try_id()` 在非 tokio 上下文返回
  /// `None`——owner 与 caller 任一为 `None` 时跳过断言（无法判定）。
  owner_task_id: Option<tokio::task::Id>,
}

impl TxnHolder {
  fn new(txn: Transaction<'static, Postgres>) -> Self {
    TxnHolder { txn, counter: 1, savepoints: Vec::new(), owner_task_id: tokio::task::try_id() }
  }

  /// Debug 期护栏：断言当前 tokio task 与创建本事务句柄的 task 一致。
  /// 不一致即「事务态句柄跨 task 并发使用」，在 debug build panic 暴露；
  /// release build 编译为空操作（`debug_assert!` 零开销）。
  fn assert_same_owner_task(&self, op: &str) {
    if let (Some(owner), Some(current)) = (self.owner_task_id, tokio::task::try_id()) {
      debug_assert!(
        owner == current,
        "TxnHolder::{op} called from tokio task {current:?} but the txn was created by task {owner:?} \
         — transaction-state DbxPostgres handle used concurrently across tasks (savepoint counter/stack will corrupt)."
      );
    }
  }

  fn inc(&mut self) -> String {
    self.assert_same_owner_task("inc");
    let savepoint_name = format!("sp_{}", self.counter);
    self.counter += 1;
    self.savepoints.push(savepoint_name.clone());
    savepoint_name
  }

  fn dec(&mut self) -> (i32, Option<String>) {
    self.assert_same_owner_task("dec");
    // counter 表示事务嵌套深度，正常情况下 commit/rollback 不会多于 begin。
    // 若下溢成负数说明 begin/commit 调用不平衡（很可能是事务态 Dbx 被跨
    // tokio task 并发使用导致），dev/test 下尽早暴露。
    debug_assert!(
      self.counter > 0,
      "TxnHolder::dec called with counter={} — unbalanced begin/commit (transaction depth underflow)",
      self.counter
    );
    self.counter -= 1;
    let savepoint = self.savepoints.pop();
    (self.counter, savepoint)
  }
}

impl Deref for TxnHolder {
  type Target = Transaction<'static, Postgres>;

  fn deref(&self) -> &Self::Target {
    &self.txn
  }
}

impl DerefMut for TxnHolder {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.txn
  }
}
