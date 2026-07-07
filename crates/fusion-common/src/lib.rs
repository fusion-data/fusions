//! crate: fusion_common
//! 常用 Rust 工具库。

pub mod ctx;
pub mod digest;
pub mod env;
pub mod error;
pub mod meta;
pub mod model;
pub mod process;
pub mod regex;
pub mod runtime;
pub mod serde;
pub mod string;
pub mod time;
#[cfg(feature = "with-uuid")]
pub mod uuid;
pub mod ahash {
  pub use ahash::*;
}

// 重新导出常用类型
pub use error::{Error, Result, codes};
