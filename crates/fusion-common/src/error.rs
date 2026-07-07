use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

// ==========================================
// 错误码常量（遵循 SPECIFICATION.md 规范）
// 格式：namespace.error_name（snake_case）
// ==========================================
//
// 这些常量为整个 fusion 生态共享的"业务错误码"。
// 真正的 `DataError` 类型定义在聚合 crate `fusions::error` 中，
// 在那里集中实现各 fusion-xxx Error 与第三方错误到 `DataError` 的 `From` 转换。

pub mod codes {
  // validation 命名空间 - 请求验证相关
  pub const INVALID_ARGUMENT: &str = "validation.invalid_argument";
  pub const BAD_REQUEST: &str = "validation.bad_request";
  pub const INVALID_PAYLOAD: &str = "validation.invalid_payload";
  /// 请求参数合法但系统当前状态不满足执行前置条件（如：必勾项未完成、已归档资源不可实例化）。
  /// 与 BAD_REQUEST（参数本身非法）区分；映射 gRPC/Connect `FailedPrecondition`。
  pub const FAILED_PRECONDITION: &str = "validation.failed_precondition";

  // auth 命名空间 - 认证相关
  pub const UNAUTHORIZED: &str = "auth.unauthorized";
  pub const INVALID_TOKEN: &str = "auth.invalid_token";
  pub const TOKEN_EXPIRED: &str = "auth.token_expired";
  pub const PERMISSION_DENIED: &str = "auth.permission_denied";

  // resource 命名空间 - 资源操作相关
  pub const NOT_FOUND: &str = "resource.not_found";
  pub const ALREADY_EXISTS: &str = "resource.already_exists";
  pub const CONFLICT: &str = "resource.conflict";

  // system 命名空间 - 系统内部错误
  pub const INTERNAL_ERROR: &str = "system.internal_error";
  pub const SERVICE_UNAVAILABLE: &str = "system.service_unavailable";
  pub const CONFIG_ERROR: &str = "system.config_error";
  pub const IO_ERROR: &str = "system.io_error";

  // rate_limit 命名空间 - 限流相关
  pub const RATE_LIMITED: &str = "rate_limit.exceeded";
  pub const RETRY_LIMIT: &str = "rate_limit.retry_limit";

  // channel 命名空间 - 通道/通信相关
  pub const CHANNEL_ERROR: &str = "channel.error";

  // rpc 命名空间 - ConnectRPC 相关
  pub const RPC_ERROR: &str = "rpc.error";
}

#[derive(Debug, Error)]
pub enum Error {
  // -- Base64
  #[error("Decode base64 fail, string is {0}")]
  FailToB64uDecode(String),

  /// base64 解码成功，但结果不是合法 UTF-8 字符串。
  #[error("Base64 decoded bytes are not valid UTF-8, string is {0}")]
  B64uDecodedNotUtf8(String),

  #[error("Parse date fail, data is {0}")]
  DateFailParse(String),

  #[error("Key fail.")]
  KeyFail,

  #[error("Password not match.")]
  PwdNotMatching,

  #[error("Missing env: {0}")]
  MissingEnv(String),

  #[error("Wrong format: {0}")]
  WrongFormat(String),
}

impl From<chrono::ParseError> for Error {
  fn from(value: chrono::ParseError) -> Self {
    Error::DateFailParse(value.to_string())
  }
}
