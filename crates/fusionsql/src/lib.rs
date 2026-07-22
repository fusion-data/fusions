pub mod base;
pub mod common;
mod config;
mod error;
pub mod field;
pub mod includes;
mod macro_helpers;
mod model_manager;
#[cfg(feature = "with-postgres")]
pub mod postgres;
#[cfg(feature = "with-sqlite")]
pub mod sqlite;
pub mod store;

pub use config::DbConfig;
pub use error::{Result, SqlError};
pub use field::Fields;
pub use filter::FilterNodes;
pub use fusion_common::ctx::Ctx;
pub use fusionsql_core::filter; // Re-export filter from fusionsql-core
pub use fusionsql_core::id; // Re-export id from fusionsql-core
pub use fusionsql_core::page; // Re-export page from fusion-common
pub use fusionsql_core::sea_utils::SIden;
pub use model_manager::{DefaultModelManager, ModelContext, ModelManager}; // Re-export Ctx from fusion-common
// Re-export sea_query：派生宏生成代码引用 `::fusionsql::sea_query::...` 绝对路径，
// 下游无需（也不应）自带一个名字恰好叫 `sea_query` 的直接依赖。
pub use sea_query;
