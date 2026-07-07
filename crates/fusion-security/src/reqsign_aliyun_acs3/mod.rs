//! 阿里云 ACS3-HMAC-SHA256 请求签名（v3）。
//!
//! 基于 [opendal-reqsign](https://github.com/apache/opendal-reqsign) 的 trait
//! 抽象（`SignRequest` / `ProvideCredential` / `SigningCredential`），实现阿里云
//! API Gateway v3 多 header 签名规范（替代 v1 RPC 风格）。
//!
//! # 签名算法
//!
//! ```text
//! canonicalRequest = HTTPMethod + "\n"
//!                  + canonicalURI + "\n"
//!                  + canonicalQueryString + "\n"
//!                  + canonicalHeaders + "\n"
//!                  + signedHeaders + "\n"
//!                  + hashedRequestPayload
//! stringToSign     = "ACS3-HMAC-SHA256" + "\n" + hex(sha256(canonicalRequest))
//! signature        = hex(HMAC-SHA256(secret_bytes, stringToSign))
//! Authorization    = "ACS3-HMAC-SHA256 Credential=<ak>,SignedHeaders=<list>,Signature=<sig>"
//! ```
//!
//! # 用法
//!
//! ```ignore
//! use fusion_security::reqsign_aliyun_acs3::{Credential, RequestSigner, StaticCredentialProvider};
//! use reqsign_core::{Context, Signer};
//! use http::Request;
//! use bytes::Bytes;
//!
//! let cred = Credential::new("ak", "sk");
//! let signer = Signer::new(Context::new(), StaticCredentialProvider::new(cred), RequestSigner::new());
//! let req = Request::builder()
//!   .method("POST").uri("https://dysmsapi.aliyuncs.com/")
//!   .header("x-acs-action", "SendSms")
//!   .header("x-acs-version", "2017-05-25")
//!   .body(Bytes::new()).unwrap();
//! let (mut parts, _body) = req.into_parts();
//! signer.sign(&mut parts, None).await?;
//! // parts.headers 现在含 Authorization / x-acs-date / x-acs-signature-nonce / x-acs-content-sha256
//! ```
//!
//! # 业务无关性
//!
//! 本模块**不绑死任何具体阿里云产品**（SMS、ECS、RAM 都可复用）；
//! 上层业务自定义 `x-acs-action` / `x-acs-version` / endpoint host / payload。

mod credential;
mod sign_request;

pub use credential::{Credential, StaticCredentialProvider};
pub use sign_request::{RequestSigner, sign_acs3};
