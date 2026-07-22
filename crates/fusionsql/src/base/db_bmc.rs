use fusionsql_core::page::StaticOrderBys;
use fusionsql_core::sea_utils::SIden;
use sea_query::{IntoIden, TableRef};

#[derive(Debug, Clone)]
pub struct BmcConfig {
  pub list_limit_default: u64,
  pub list_limit_max: u64,
  pub table: &'static str,
  pub schema: Option<&'static str>,
  pub column_id: &'static str,
  pub id_generated_by_db: bool,
  pub has_created_by: bool,
  pub has_created_at: bool,
  pub has_updated_by: bool,
  pub has_updated_at: bool,
  pub use_logical_deletion: bool,
  pub has_owner_id: bool,
  pub has_optimistic_lock: bool,
  pub order_bys: Option<StaticOrderBys>,
  /// 排序列显式白名单（覆盖实体列默认名单）。
  ///
  /// `OrderBy` 的列名直接拼进无法参数化的 `ORDER BY` 子句（`to_sql()` 仅做引号
  /// 转义），`From<&str>` 构造期不做任何校验。分页 / 列表查询路径
  /// （`compute_page`）会校验客户端提交的每个 `OrderBy` 列名，非法即返回
  /// [`crate::SqlError::InvalidArgument`]。
  ///
  /// **`None` 时按实体列集合校验（opt-out 安全默认）**：名单回落为
  /// `HasFields::field_names()`，客户端最多只能按实体自身的列排序。设置此字段
  /// 用于两类场景：比实体列集合更收紧（实体列中仍有不宜作排序侧信道的敏感列），
  /// 或显式放开 join / 计算列。服务端默认排序（[`Self::with_order_bys`]）是受信
  /// 配置，不经过校验。
  pub order_by_allowlist: Option<&'static [&'static str]>,
}

impl BmcConfig {
  /// 创建 BMC 配置，所有审计列默认 **false**。
  ///
  /// 之前默认全 `true`（has_created_by / has_created_at / has_updated_by /
  /// has_updated_at），新建一张只有 `(id, name)` 的简单表 → 自动 BMC `insert`
  /// SQL 带不存在的 `created_by` 列 → 运行时 SQL 错指向 sea-query 内部，caller
  /// 一头雾水。改为默认全关，需要审计列的表用 [`Self::with_audit_columns`]
  /// 一键打开 4 个。
  #[must_use]
  pub fn new(table_name: &'static str, schema: Option<&'static str>) -> Self {
    Self {
      table: table_name,
      schema,
      list_limit_default: super::LIST_LIMIT_DEFAULT,
      list_limit_max: super::LIST_LIMIT_MAX,
      column_id: "id",
      id_generated_by_db: false,
      has_created_by: false,
      has_created_at: false,
      has_updated_by: false,
      has_updated_at: false,
      use_logical_deletion: false,
      has_owner_id: false,
      has_optimistic_lock: false,
      order_bys: None,
      order_by_allowlist: None,
    }
  }

  #[must_use]
  pub fn new_table(table_name: &'static str) -> Self {
    Self::new(table_name, None)
  }

  /// 一键启用 4 个标准审计列：`created_by` / `created_at` / `updated_by` / `updated_at`。
  /// 等价于 `with_has_created_by(true).with_has_created_at(true).with_has_updated_by(true).with_has_updated_at(true)`。
  #[must_use]
  pub fn with_audit_columns(mut self) -> Self {
    self.has_created_by = true;
    self.has_created_at = true;
    self.has_updated_by = true;
    self.has_updated_at = true;
    self
  }

  #[must_use]
  pub fn with_list_limit_default(mut self, list_limit_default: u64) -> Self {
    self.list_limit_default = list_limit_default;
    self
  }

  #[must_use]
  pub fn with_list_limit_max(mut self, list_limit_max: u64) -> Self {
    self.list_limit_max = list_limit_max;
    self
  }

  #[must_use]
  pub fn with_column_id(mut self, column_id: &'static str) -> Self {
    self.column_id = column_id;
    self
  }

  #[must_use]
  pub fn with_id_generated_by_db(mut self, id_generated_by_db: bool) -> Self {
    self.id_generated_by_db = id_generated_by_db;
    self
  }

  #[must_use]
  pub fn with_has_created_by(mut self, has_created_by: bool) -> Self {
    self.has_created_by = has_created_by;
    self
  }

  #[must_use]
  pub fn with_has_created_at(mut self, has_created_at: bool) -> Self {
    self.has_created_at = has_created_at;
    self
  }

  #[must_use]
  pub fn with_has_updated_by(mut self, has_updated_by: bool) -> Self {
    self.has_updated_by = has_updated_by;
    self
  }

  #[must_use]
  pub fn with_has_updated_at(mut self, has_updated_at: bool) -> Self {
    self.has_updated_at = has_updated_at;
    self
  }

  #[must_use]
  pub fn with_use_logical_deletion(mut self, use_logical_deletion: bool) -> Self {
    self.use_logical_deletion = use_logical_deletion;
    self
  }

  #[must_use]
  pub fn with_has_owner_id(mut self, has_owner_id: bool) -> Self {
    self.has_owner_id = has_owner_id;
    self
  }

  #[must_use]
  pub fn with_has_optimistic_lock(mut self, has_optimistic_lock: bool) -> Self {
    self.has_optimistic_lock = has_optimistic_lock;
    self
  }

  #[must_use]
  pub fn with_order_bys(mut self, order_bys: Option<StaticOrderBys>) -> Self {
    self.order_bys = order_bys;
    self
  }

  /// 声明排序列显式白名单，**覆盖**默认的实体列集合名单。
  ///
  /// 未调用此方法的 BMC 默认按 `HasFields::field_names()` 校验客户端排序列
  /// （opt-out 安全默认）；本方法用于比实体列更收紧，或显式放开 join / 计算列。
  /// 列名应为剥离 `!` 降序前缀后的裸列名（校验时会先 `OrderBy::parse()`）。
  #[must_use]
  pub fn with_order_by_allowlist(mut self, allowlist: &'static [&'static str]) -> Self {
    self.order_by_allowlist = Some(allowlist);
    self
  }

  #[must_use]
  pub fn table_ref(&self) -> TableRef {
    match self.schema {
      Some(schema) => TableRef::SchemaTable(SIden(schema).into_iden(), SIden(self.table).into_iden()),
      None => TableRef::Table(SIden(self.table).into_iden()),
    }
  }

  #[must_use]
  pub fn qualified_table(&self) -> (&'static str, &'static str) {
    (self.schema.unwrap_or("public"), self.table)
  }

  #[must_use]
  pub fn qualified_table_name(&self) -> String {
    match self.schema {
      Some(schema) => format!("{}.{}", schema, self.table),
      None => self.table.to_string(),
    }
  }
}

// /// 注意，暂未使用
// #[derive(Debug, Clone, Default)]
// pub struct DynBmcConfig {
//   pub order_bys: Option<OrderBys>,
// }

// impl DynBmcConfig {
//   pub fn with_order_bys(mut self, order_bys: Option<OrderBys>) -> Self {
//     self.order_bys = order_bys;
//     self
//   }
// }

/// The `DbBmc` trait must be implemented for the Bmc struct of an entity.
/// It specifies meta information such as the table name,
/// whether the table has timestamp columns (`created_by`, `created_at`, `updated_by`, `updated_at`), and more as the
/// code evolves.
///
/// Note: This trait should not be confused with the `BaseCrudBmc` trait, which provides
///       common default CRUD BMC functions for a given Bmc/Entity.
pub trait DbBmc {
  /// BMC 元配置（表名 / 审计列 / 逻辑删除 / 排序 allowlist 等）。
  ///
  /// 前导下划线是本 trait 的 **protected 约定**：该方法由实现方提供（通常配合
  /// `OnceLock<BmcConfig>`），仅供 `fusionsql::base::*` CRUD 框架函数读取；
  /// 业务代码 MUST NOT 直接调用它做逻辑判断。下划线即"实现它、别调它"的信号，
  /// 不是命名遗留。
  fn _bmc_config() -> &'static BmcConfig;
  // fn _dynamic_config() -> DynBmcConfig {
  //   DynBmcConfig::default()
  // }
}
