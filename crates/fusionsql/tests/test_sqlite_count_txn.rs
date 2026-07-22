//! 回归：SQLite `count` / `count_on` 必须走事务感知路径。
//!
//! 旧实现直接对连接池 `fetch_one`，绕过当前打开的事务 —— file 库下读到事务前
//! 快照（count 偏小）或撞写锁报错；`:memory:` 下各连接是独立数据库，结果必错。
#![cfg(feature = "with-sqlite")]

use std::sync::OnceLock;

use fusionsql::base::{self, BmcConfig, DbBmc};
use fusionsql::field::Fields;
use fusionsql::filter::{FilterGroups, FilterNode};
use fusionsql::store::Dbx;
use fusionsql::{Ctx, DbConfig, ModelManager};
use serde_json::json;

/// 「无过滤条件」= 单个空 group（AND 空集 → TRUE）。注意不能用
/// `FilterGroups(Vec::new())`：groups 之间是 OR，OR 空集渲染为 FALSE。
fn no_filter() -> FilterGroups {
  FilterGroups::from(Vec::<FilterNode>::new())
}

#[derive(Fields)]
struct UserForCreate {
  name: String,
  status: i32,
}

struct UserBmc;
impl DbBmc for UserBmc {
  fn _bmc_config() -> &'static BmcConfig {
    static CONFIG: OnceLock<BmcConfig> = OnceLock::new();
    CONFIG.get_or_init(|| BmcConfig::new_table("user").with_id_generated_by_db(true))
  }
}

#[tokio::test]
async fn test_sqlite_count_sees_rows_inside_transaction() {
  let db_path =
    format!("file:{}?mode=rwc", std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("count_txn.db").display());
  let db_config: DbConfig = serde_json::from_value(json!({
    "enable": true,
    "url": db_path,
    "max_connections": 5,
    "min_connections": 1,
    "acquire_timeout": "10s",
  }))
  .unwrap();

  let mm = ModelManager::<Ctx>::new(&db_config, Some("test-sqlite-count"))
    .await
    .unwrap()
    .with_ctx(Ctx::new_super_admin());

  // ModelManager 手写 Debug：下游 `#[derive(Debug)]` 包装可编译，且不 dump 连接细节
  let dbg = format!("{mm:?}");
  assert!(dbg.contains("ModelManager") && dbg.contains("Sqlite"), "unexpected Debug output: {dbg}");

  match mm.dbx() {
    Dbx::Sqlite(dbx) => {
      sqlx::query("DROP TABLE IF EXISTS user").execute(dbx.db()).await.unwrap();
      sqlx::query("CREATE TABLE user (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, status INT NOT NULL)")
        .execute(dbx.db())
        .await
        .unwrap();
    }
    #[allow(unreachable_patterns)]
    _ => panic!("expected sqlite dbx"),
  }

  base::create::<Ctx, UserBmc, _>(&mm, UserForCreate { name: "outside".into(), status: 1 })
    .await
    .unwrap();

  let in_txn_count = mm
    .transaction(|mm| async move {
      base::create::<Ctx, UserBmc, _>(&mm, UserForCreate { name: "inside".into(), status: 1 }).await?;
      base::count::<Ctx, UserBmc, _>(&mm, no_filter()).await
    })
    .await
    .unwrap();

  assert_eq!(in_txn_count, 2, "count inside txn must see the uncommitted row");

  let after_commit = base::count::<Ctx, UserBmc, _>(&mm, no_filter()).await.unwrap();
  assert_eq!(after_commit, 2);
}
