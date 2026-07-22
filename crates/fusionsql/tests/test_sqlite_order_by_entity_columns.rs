//! 端到端回归：客户端 ORDER BY 列名默认按实体列集合校验（opt-out 安全默认）。
//!
//! - 实体列（含 `!` 降序前缀）可排序；
//! - 表里真实存在但实体未映射的隐藏列（如密文列）被拒绝 —— 挡 ORDER BY
//!   oracle 侧信道与 schema 探测；
//! - 显式 `with_order_by_allowlist` 覆盖实体列默认名单（更收紧）。
#![cfg(feature = "with-sqlite")]

use std::sync::OnceLock;

use fusionsql::base::{self, BmcConfig, DbBmc};
use fusionsql::field::Fields;
use fusionsql::filter::{FilterGroups, FilterNode};
use fusionsql::page::{OrderBy, OrderBys, Page};
use fusionsql::store::Dbx;
use fusionsql::{Ctx, DbConfig, ModelManager, SqlError};
use serde_json::json;
use sqlx::FromRow;

#[derive(Debug, FromRow, Fields)]
struct User {
  id: i64,
  name: String,
  status: i32,
}

#[derive(Fields)]
struct UserForCreate {
  name: String,
  status: i32,
  secret: String,
}

struct UserBmc;
impl DbBmc for UserBmc {
  fn _bmc_config() -> &'static BmcConfig {
    static CONFIG: OnceLock<BmcConfig> = OnceLock::new();
    CONFIG.get_or_init(|| BmcConfig::new_table("user").with_id_generated_by_db(true))
  }
}

/// 与 UserBmc 同表，但显式 allowlist 收紧到只允许按 id 排序。
struct UserIdOnlyBmc;
impl DbBmc for UserIdOnlyBmc {
  fn _bmc_config() -> &'static BmcConfig {
    static CONFIG: OnceLock<BmcConfig> = OnceLock::new();
    CONFIG.get_or_init(|| BmcConfig::new_table("user").with_id_generated_by_db(true).with_order_by_allowlist(&["id"]))
  }
}

fn no_filter() -> FilterGroups {
  FilterGroups::from(Vec::<FilterNode>::new())
}

fn page_order_by(col: &str) -> Option<Page> {
  Some(Page { order_bys: Some(OrderBys::from(OrderBy::from(col.to_string()))), ..Default::default() })
}

async fn find_many(mm: &ModelManager<Ctx>, page: Option<Page>) -> Result<Vec<User>, SqlError> {
  base::sqlite_find_many::<Ctx, UserBmc, User, _>(mm, no_filter(), page).await
}

#[tokio::test]
async fn test_order_by_validated_against_entity_columns() {
  let db_path =
    format!("file:{}?mode=rwc", std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("order_by.db").display());
  let db_config: DbConfig = serde_json::from_value(json!({
    "enable": true,
    "url": db_path,
    "max_connections": 5,
    "min_connections": 1,
    "acquire_timeout": "10s",
  }))
  .unwrap();

  let mm = ModelManager::<Ctx>::new(&db_config, Some("test-sqlite-order-by"))
    .await
    .unwrap()
    .with_ctx(Ctx::new_super_admin());

  match mm.dbx() {
    Dbx::Sqlite(dbx) => {
      sqlx::query("DROP TABLE IF EXISTS user").execute(dbx.db()).await.unwrap();
      // `secret` 列真实存在于表中,但实体 `User` 未映射它
      sqlx::query(
        "CREATE TABLE user (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, status INT NOT NULL, secret TEXT NOT NULL)",
      )
      .execute(dbx.db())
      .await
      .unwrap();
    }
    #[allow(unreachable_patterns)]
    _ => panic!("expected sqlite dbx"),
  }

  for (name, status, secret) in [("bob", 2, "s1"), ("alice", 1, "s2")] {
    base::create::<Ctx, UserBmc, _>(&mm, UserForCreate { name: name.into(), status, secret: secret.into() })
      .await
      .unwrap();
  }

  // 实体列可排序（含 `!` 降序前缀）
  let users = find_many(&mm, page_order_by("name")).await.unwrap();
  assert_eq!(users.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), vec!["alice", "bob"]);
  let users = find_many(&mm, page_order_by("!status")).await.unwrap();
  assert_eq!(users.iter().map(|u| u.status).collect::<Vec<_>>(), vec![2, 1]);

  // 隐藏列 `secret` 在表中存在但不在实体列集合内 → 拒绝（不是数据库报错,是入口校验）
  let err = find_many(&mm, page_order_by("secret")).await.unwrap_err();
  assert!(matches!(err, SqlError::InvalidArgument { .. }), "unexpected: {err:?}");

  // 不存在的列同样在入口被拒,不触达数据库（无 schema 探测面）
  let err = find_many(&mm, page_order_by("no_such_column")).await.unwrap_err();
  assert!(matches!(err, SqlError::InvalidArgument { .. }), "unexpected: {err:?}");

  // 显式 allowlist 覆盖实体列默认名单：name 在实体列内但不在名单内 → 拒绝
  let err = base::sqlite_find_many::<Ctx, UserIdOnlyBmc, User, _>(&mm, no_filter(), page_order_by("name"))
    .await
    .unwrap_err();
  assert!(matches!(err, SqlError::InvalidArgument { .. }), "unexpected: {err:?}");
  let users = base::sqlite_find_many::<Ctx, UserIdOnlyBmc, User, _>(&mm, no_filter(), page_order_by("id"))
    .await
    .unwrap();
  assert_eq!(users.len(), 2);
}
