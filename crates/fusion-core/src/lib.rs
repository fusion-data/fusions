//! fusion-core 核心库

pub mod application;
pub mod component;
pub mod concurrent;
pub mod configuration;
pub mod error;
#[cfg(feature = "with-logforth")]
pub mod logforth;
pub mod meta;
pub mod plugin;
mod run_mode;
pub mod security;
pub mod signal;
pub mod timer;
#[cfg(feature = "with-tracing")]
pub mod tracing;
pub mod utils;

pub use async_trait::async_trait;
#[cfg(feature = "with-macros")]
pub use fusion_core_macros::Builder;
pub use run_mode::*;

pub use application::Application;
pub use error::{CoreError, CoreResult};

/// fusion-core 默认 Result 别名，错误类型为 [`CoreError`]。
pub type Result<T> = core::result::Result<T, CoreError>;
