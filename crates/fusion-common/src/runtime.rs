use std::{
  env::{self, VarError},
  path::PathBuf,
};

pub static CARGO_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub type Result<T> = core::result::Result<T, VarError>;

#[inline]
pub fn cargo_manifest_dir() -> Result<PathBuf> {
  from_env("CARGO_MANIFEST_DIR").map(PathBuf::from)
}

#[inline]
pub fn cargo_pkg_name() -> Result<String> {
  from_env("CARGO_PKG_NAME")
}

#[inline]
pub fn cargo_pkg_version() -> Result<String> {
  from_env("CARGO_PKG_VERSION")
}

#[inline]
fn from_env(name: &str) -> Result<String> {
  env::var(name)
}

/// 把 bin crate 编译期 `CARGO_MANIFEST_DIR` 写入 process env。
///
/// fusion 配置加载（`fusion_core::configuration::util::load_from_files`）通过
/// `cargo_manifest_dir()` 运行时 `env::var` 读 `CARGO_MANIFEST_DIR` 来定位
/// `<bin_crate>/resources/app.toml`。`cargo run` 自动注入该 env；裸二进制启动
/// （systemd / 直接运行 `target/debug/<bin>` / `target/release/<bin>`）则没有，
/// 导致 fallback 走 cwd 才能找到配置——并非所有启动方式都保证 cwd 正确。
///
/// 调用方（bin crate `main.rs`）在 `main()` 最早期用 `env!("CARGO_MANIFEST_DIR")`
/// 把编译期值传进来，从此函数把该值写入 process env，让后续配置加载找到真理源路径。
///
/// 幂等：cargo run 已注入同名 env 时，覆盖为编译期值，dev 路径下两值等价。
///
/// # 约束
///
/// MUST 在 `main()` 函数体最早期、tokio runtime 启动前调用（此时单线程）。
/// `set_env` 内部封装 `std::env::set_var`（Rust 2024 起 unsafe），调用方需保证
/// 未并发读 env；本函数约定仅启动早期调用，故满足该前提。
pub fn init_compiled_manifest_dir(dir: &'static str) {
  // SAFETY: 调用约定在 main 最早期单线程下；environ 写不与并发读撞车。
  let _ = crate::env::set_env("CARGO_MANIFEST_DIR", dir);
}
