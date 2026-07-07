//! ACS3 凭据 + StaticCredentialProvider（实现 reqsign-core 的 SigningCredential / ProvideCredential trait）

use reqsign_core::{Context, ProvideCredential, Result, SigningCredential};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// 阿里云 ACS3 签名凭据。
///
/// `security_token` 当用户使用 STS 临时凭据时填入（会自动加到 `x-acs-security-token` header）。
///
/// **安全模型**：
/// - 字段 `pub(crate)`：外部 caller 通过 [`Self::new`] 构造，避免 struct-update 漏赋值时
///   遗留默认空值；如需读取走 [`Self::access_key_id`] / [`Self::access_key_secret`]
///   getter（密钥读出仍是 `&str`，但接口面收紧）
/// - `Debug` 自定义实现 mask `access_key_secret` 与 `security_token`：默认 derive
///   会把 secret 直接打印到 `tracing::error!("{:?}", cred)` / panic backtrace
/// - `ZeroizeOnDrop`：drop 时清零内存，防 swap 文件 / coredump 残留密钥
///
/// **AK/SK 顺序提醒**：[`Self::new`] 接受两个 `String`，写反 `Credential::new(sk, ak)`
/// 编译通过 `is_valid()` 也返回 true，但阿里云会以 401 拒绝。建议从 env / 配置文件
/// 读时通过 dedicated key 名（如 `ALIYUN_ACCESS_KEY_ID` / `ALIYUN_ACCESS_KEY_SECRET`）
/// 区分而非位置参数；[`Self::from_env`] 提供该 helper。
#[derive(Clone, ZeroizeOnDrop, Default)]
pub struct Credential {
  pub(crate) access_key_id: String,
  pub(crate) access_key_secret: String,
  pub(crate) security_token: Option<String>,
}

impl Credential {
  pub fn new(access_key_id: impl Into<String>, access_key_secret: impl Into<String>) -> Self {
    Self { access_key_id: access_key_id.into(), access_key_secret: access_key_secret.into(), security_token: None }
  }

  pub fn with_security_token(mut self, token: impl Into<String>) -> Self {
    self.security_token = Some(token.into());
    self
  }

  /// Read AK/SK from env vars by key name. Avoids the positional-argument
  /// AK-vs-SK swap footgun in [`Self::new`].
  pub fn from_env(ak_var: &str, sk_var: &str) -> Option<Self> {
    let ak = std::env::var(ak_var).ok().filter(|s| !s.is_empty())?;
    let sk = std::env::var(sk_var).ok().filter(|s| !s.is_empty())?;
    Some(Self::new(ak, sk))
  }

  pub fn access_key_id(&self) -> &str {
    &self.access_key_id
  }

  pub fn access_key_secret(&self) -> &str {
    &self.access_key_secret
  }

  pub fn security_token(&self) -> Option<&str> {
    self.security_token.as_deref()
  }
}

impl std::fmt::Debug for Credential {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // Mask secret material; never include access_key_secret / security_token
    // in Debug output. access_key_id is OK to print (used for log correlation).
    f.debug_struct("Credential")
      .field("access_key_id", &self.access_key_id)
      .field("access_key_secret", &"<redacted>")
      .field("security_token", &self.security_token.as_ref().map(|_| "<redacted>"))
      .finish()
  }
}

impl SigningCredential for Credential {
  fn is_valid(&self) -> bool {
    !self.access_key_id.is_empty() && !self.access_key_secret.is_empty()
  }
}

impl Zeroize for Credential {
  fn zeroize(&mut self) {
    self.access_key_id.zeroize();
    self.access_key_secret.zeroize();
    if let Some(t) = self.security_token.as_mut() {
      t.zeroize();
    }
  }
}

/// 静态凭据 Provider —— 直接持有 AK/SK。
///
/// 进程级长期凭据用此；STS 临时凭据 / Profile / EcsRamRole 等 provider 后续按需扩展。
#[derive(Debug, Clone)]
pub struct StaticCredentialProvider {
  credential: Credential,
}

impl StaticCredentialProvider {
  pub fn new(credential: Credential) -> Self {
    Self { credential }
  }
}

impl ProvideCredential for StaticCredentialProvider {
  type Credential = Credential;

  async fn provide_credential(&self, _ctx: &Context) -> Result<Option<Self::Credential>> {
    if self.credential.is_valid() { Ok(Some(self.credential.clone())) } else { Ok(None) }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn credential_is_valid_requires_both_ak_and_sk() {
    assert!(Credential::new("ak", "sk").is_valid());
    assert!(!Credential::new("", "sk").is_valid());
    assert!(!Credential::new("ak", "").is_valid());
    assert!(!Credential::default().is_valid());
  }

  #[test]
  fn credential_with_security_token_preserves_ak_sk() {
    let cred = Credential::new("ak", "sk").with_security_token("tok");
    assert_eq!(cred.access_key_id(), "ak");
    assert_eq!(cred.access_key_secret(), "sk");
    assert_eq!(cred.security_token(), Some("tok"));
  }

  #[test]
  fn credential_debug_masks_secrets() {
    let cred = Credential::new("ak123", "sk-super-secret").with_security_token("tok-xyz");
    let dbg = format!("{cred:?}");
    assert!(dbg.contains("ak123"), "access_key_id should be visible for log correlation");
    assert!(!dbg.contains("sk-super-secret"), "access_key_secret leaked: {dbg}");
    assert!(!dbg.contains("tok-xyz"), "security_token leaked: {dbg}");
    assert!(dbg.contains("<redacted>"));
  }

  #[tokio::test]
  async fn static_provider_returns_credential_when_valid() {
    let provider = StaticCredentialProvider::new(Credential::new("ak", "sk"));
    let ctx = Context::new();
    let cred = provider.provide_credential(&ctx).await.unwrap();
    assert!(cred.is_some());
  }

  #[tokio::test]
  async fn static_provider_returns_none_when_invalid() {
    let provider = StaticCredentialProvider::new(Credential::default());
    let ctx = Context::new();
    let cred = provider.provide_credential(&ctx).await.unwrap();
    assert!(cred.is_none());
  }
}
