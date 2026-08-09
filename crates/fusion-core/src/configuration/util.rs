use std::{collections::HashMap, env, path::Path};

use config::{Config, ConfigBuilder, Environment, File, FileFormat, builder::DefaultState};
use fusion_common::{
  env::{get_env, get_envs},
  runtime,
};
use log::{debug, trace};

use super::ConfigureResult;

/// 加载配置
///
/// [crate::RunModel]
pub fn load_config() -> ConfigureResult<Config> {
  load_config_with(None)
}

pub fn load_config_with(custom_config: Option<Config>) -> ConfigureResult<Config> {
  let mut b = Config::builder().add_source(load_default_source());

  // load from default files, if exists
  b = load_from_files(&["app.toml".to_string(), "app.yaml".to_string(), "app.yml".to_string()], b);

  // load from profile files, if exists
  let profile_files = if let Ok(profiles_active) = env::var("FUSION__PROFILES__ACTIVE") {
    vec![
      format!("app-{profiles_active}.toml"),
      format!("app-{profiles_active}.yaml"),
      format!("app-{profiles_active}.yml"),
    ]
  } else {
    vec![]
  };
  debug!("Load profile files: {:?}", profile_files);
  b = load_from_files(&profile_files, b);

  // load from file of env, if exists
  if let Ok(file) = get_env("FUSION_CONFIG_FILE") {
    log::info!("Load config from env FUSION_CONFIG_FILE: {}", file);
    let path = Path::new(&file);
    if path.exists() {
      log::info!("FUSION_CONFIG_FILE exists, load config from path: {}", path.display());
      b = b.add_source(File::from(path));
    }
  }

  b = add_environment(b);

  if let Some(custom_config) = custom_config {
    b = b.add_source(custom_config);
  }

  let c = b.build()?;

  trace!("Load config file: {}", c.cache);

  Ok(c)
}

fn load_from_files(files: &[String], mut b: ConfigBuilder<DefaultState>) -> ConfigBuilder<DefaultState> {
  for file in files {
    if let Ok(path) = runtime::cargo_manifest_dir().map(|dir| dir.join("resources").join(file))
      && path.exists()
    {
      b = b.add_source(File::from(path));
      break;
    }
  }

  for file in files {
    let path = Path::new(file);
    if path.exists() {
      b = b.add_source(File::from(path));
      break;
    }
  }

  b
}

pub fn load_default_source() -> File<config::FileSourceString, FileFormat> {
  let text = include_str!("default.toml");
  File::from_str(text, FileFormat::Toml)
}

pub fn add_environment(b: ConfigBuilder<DefaultState>) -> ConfigBuilder<DefaultState> {
  // Load all latest variables from current environment
  let envs = get_envs();
  let env = Environment::default().separator("__").source(Some(envs.into_iter().collect::<HashMap<_, _>>()));
  b.add_source(env)
}

/// 测试专用 mutex：所有触碰 process-wide env 的 unit test 走此锁串行化。
///
/// 背景：`fusion_common::env::set_env` / `remove_env` 内部用 `unsafe std::env::set_var`
/// / `remove_var` 改全局 environ；同 binary 内 `load_config()` 经
/// `get_envs() → env::vars()` 并发读 environ，setenv/getenv 在 libc 层非线程安全，
/// 与读并发时 environ table 瞬间不一致 → 写入丢失，测试看到默认配置而非环境变量。
///
/// 用法：
/// ```ignore
/// let _guard = fusion_core::configuration::util::test_env_lock().lock().unwrap();
/// set_env("FUSION__APP__NAME", "x").unwrap();
/// let c = load_config().unwrap();
/// ```
///
/// 同 crate 内所有候选点（`configuration::effect`、`security::jose::helper`、
/// `application::tests::test_application_run`）必须一致获锁，否则锁形同虚设。
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
  static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  &LOCK
}

#[cfg(test)]
mod tests {
  use fusion_common::env::set_env;

  use crate::configuration::load_config;

  /// [`add_environment`] 的 env-override 契约：`Environment::default().separator("__")` 把 env 名
  /// 小写、`__` 拆 nested 层、段内单 `_` 保留。支撑 API 测试用
  /// `MYAPP__IDENTITY__AUTH__OAUTH_FEISHU_API_BASE` 覆盖 `myapp.identity.auth.oauth_feishu_api_base`
  /// （mock provider 注入），无需改 gitignored app.toml。
  #[test]
  fn add_environment_double_underscore_nests_and_single_underscore_kept() {
    let _guard = crate::configuration::test_env_lock().lock().unwrap();
    set_env("ZZTEST__ALPHA__BETA_GAMMA", "override-value").unwrap();
    let c = load_config().unwrap();
    let v: String = c.get("zztest.alpha.beta_gamma").unwrap();
    assert_eq!(v, "override-value");
  }
}
