use std::sync::OnceLock;

use regex::Regex;

static REGEX_EMAIL: &str = r"^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$";

static REGEX_PHONE_ON_CHINA: &str = r"^1[3-9]\d{9}$";

/// 校验邮件地址是否有效.
pub fn is_email(email: &str) -> bool {
  static RE: OnceLock<Regex> = OnceLock::new();
  let re = RE.get_or_init(|| Regex::new(REGEX_EMAIL).unwrap());
  re.is_match(email)
}

/// 校验手机号地址是否有效。
pub fn is_phone(phone: &str) -> bool {
  static RE: OnceLock<Regex> = OnceLock::new();
  let re = RE.get_or_init(|| Regex::new(REGEX_PHONE_ON_CHINA).unwrap());
  re.is_match(phone)
}
