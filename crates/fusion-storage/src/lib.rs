//! fusion-storage: 对象存储机制层（opendal Operator 工厂 + 预签名 URL）。
//!
//! 只收机制不收业务（framework-conventions §7）：
//! - [`build_operator`]：fs / oss / s3 / obs 四后端分支，feature 透传（feature → opendal
//!   `services-*` 映射表见 crate README）；未启用的后端返回指向 feature 的错误。
//! - 预签名：云后端走 opendal native presign（capability gate 自动分流，fs→云切换零代码
//!   改动），fs 后端走 HMAC 签名的本地路由 URL。
//!
//! 留在消费方（crate 刻意不收）：storage key 约定、公开访问 base 的 env 语义、路由挂载、
//! HMAC 密钥装配与默认密钥、默认 root、TTL 策略。fs 路由 URL 形态经 [`FsPresignRoutes`]
//! 由消费方注入——URL 组装机制在 crate，路径字面量（挂载点）在消费方。

mod config;
mod hmac;
mod operator;
mod presign;

pub use config::StorageConfig;
pub use hmac::{sign_read, sign_upload, verify_hmac, verify_upload_hmac};
pub use operator::build_operator;
pub use presign::{
  FsPresignRoutes, content_disposition_attachment, generate_signed_download_url, generate_signed_upload_url,
};
