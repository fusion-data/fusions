//! fusions - Rust 数据融合平台聚合包
//!
//! # 简介
//! fusions 聚合了 fusion-common、fusion-core 及各功能模块，提供一站式的使用体验。
//!
//! # 使用方式
//!
//! ## 基础使用
//! ```ignore
//! use fusions::core::Application;
//! use fusions::common::time::now_offset;
//! ```
//!
//! ## Web 服务
//! ```ignore
//! use fusions::{web::Router, core::Application};
//! ```
//!
//! ## 完整功能
//! ```ignore
//! // Cargo.toml 添加
//! fusions = { version = "0.2", features = ["full"] }
//! ```

// ==================== 模块 re-export ====================

/// 基础工具模块 (fusion-common)
pub use fusion_common as common;

/// 核心框架模块 (fusion-core)
pub use fusion_core as core;

/// 宏定义模块 (fusion-core-macros)
pub use fusion_core_macros as macros;

// ==================== 功能模块 re-export ====================

#[cfg(feature = "ai")]
/// AI 模块 (fusion-ai)
pub use fusion_ai as ai;

#[cfg(feature = "db")]
/// 数据库模块 (fusion-db)
pub use fusion_db as db;

#[cfg(feature = "rpc")]
/// ConnectRPC 模块 (fusion-rpc)
pub use fusion_rpc as rpc;

#[cfg(feature = "security")]
/// 安全认证模块 (fusion-security)
pub use fusion_security as security;

#[cfg(feature = "weixin")]
/// 微信登录编排模块 (fusion-weixin)
pub use fusion_weixin as weixin;

#[cfg(feature = "web")]
/// Web 框架模块 (fusion-web)
pub use fusion_web as web;

// ==================== SQL 模块 re-export ====================

#[cfg(feature = "db")]
/// SQL 模块 (fusion-sql)
pub use fusion_sql as sql;

// ==================== 子模块 ====================

pub mod error;

#[cfg(feature = "web")]
pub mod web_utils;

// ==================== 基础类型 re-export ====================

/// 业务错误模型与 Result 类型别名
pub use error::{DataError, DataResult, Result};

/// 错误码常量集（继承自 `fusion_common::codes`）
pub use fusion_common::codes;
