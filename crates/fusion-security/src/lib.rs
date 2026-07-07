//! fusion-security 安全模块

pub mod error;
pub mod jwt;

#[cfg(feature = "with-oauth")]
pub mod oauth;

#[cfg(feature = "with-aliyun-acs3")]
pub mod reqsign_aliyun_acs3;

pub use error::{SecurityError, SecurityResult};

#[cfg(feature = "with-oauth")]
pub use oauth2;
#[cfg(feature = "with-openid")]
pub use openidconnect;
