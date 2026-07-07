use std::env;
use std::str::FromStr;

use crate::digest::b64u_decode;
use crate::error::Error;

pub fn get_env(name: &str) -> Result<String, Error> {
  env::var(name).map_err(|_| Error::MissingEnv(name.to_string()))
}

pub fn get_env_parse<T: FromStr>(name: &str) -> Result<T, Error> {
  let val = get_env(name)?;
  val.parse::<T>().map_err(|_| Error::WrongFormat(name.to_string()))
}

pub fn get_envs() -> Vec<(String, String)> {
  std::env::vars().collect()
}

pub fn get_env_b64u_as_u8s(name: &str) -> Result<Vec<u8>, Error> {
  b64u_decode(&get_env(name)?).map_err(|_| Error::WrongFormat(name.to_string()))
}

/// 设置进程环境变量。
///
/// 保留 `Result` 返回类型以兼容既有 `.unwrap()` 调用点，实际永远返回 `Ok`。
///
/// # Safety
///
/// `std::env::set_var` 在多线程下是未定义行为（修改进程 environ 与其他线程的
/// `getenv` / `env::var` 读取并发即 data race，并非可被 `catch_unwind` 捕获的
/// panic）。**调用方必须保证**：仅在单线程的进程启动早期（任何会读取 env 的
/// 线程被 spawn 之前），或在已用专用 mutex 串行化所有 env 读写的测试中调用。
pub fn set_env(name: &str, value: &str) -> Result<(), Error> {
  // SAFETY: 见函数级 `# Safety` —— 由调用方保证单线程早期 / 测试串行化前提。
  unsafe {
    env::set_var(name, value);
  }
  Ok(())
}

/// 移除进程环境变量。
///
/// 保留 `Result` 返回类型以兼容既有 `.unwrap()` 调用点，实际永远返回 `Ok`。
///
/// # Safety
///
/// 与 [`set_env`] 同：`std::env::remove_var` 多线程下是 UB。调用方必须保证仅在
/// 单线程启动早期或测试串行化前提下调用。
pub fn remove_env(name: &str) -> Result<(), Error> {
  // SAFETY: 见函数级 `# Safety` —— 由调用方保证单线程早期 / 测试串行化前提。
  unsafe {
    env::remove_var(name);
  }
  Ok(())
}

/// 批量移除环境变量，参见 [`remove_env`] 的 `# Safety` 约束。
pub fn remove_envs(names: &[&str]) -> Result<(), Error> {
  for name in names {
    remove_env(name)?;
  }
  Ok(())
}
