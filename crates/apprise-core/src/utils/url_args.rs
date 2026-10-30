//! URL helpers with upstream semantics, used by the raw-socket plugins
//! (syslog, rsyslog, APRS, Growl, SMPP, IRC and XMPP).
//!
//! Upstream applies these in `URLBase` and `apprise.utils.parse`; the Rust
//! `ParsedUrl` does not, so the plugins call them while parsing their URL.

use crate::utils::parse::ParsedUrl;

/// Parse a boolean the way upstream `parse_bool(arg, default)` does: the
/// first two characters decide (`no`, `of`, `fa`, `ne`, `di`, `de`, `f`, `n`,
/// `0` are false; `ye`, `on`, `tr`, `al`, `en`, `t`, `y`, `1` are true) and
/// anything else, including an empty value, returns `default`.
pub fn parse_bool_with_default(s: &str, default: bool) -> bool {
  let lower = s.to_lowercase();
  let prefix: String = lower.chars().take(2).collect();
  match prefix.as_str() {
    "de" | "di" | "ne" | "f" | "n" | "no" | "of" | "0" | "fa" => false,
    "en" | "al" | "t" | "y" | "ye" | "on" | "1" | "tr" => true,
    _ => default,
  }
}

/// The user and password of a URL after upstream's query overrides
/// (`URLBase.post_process_parse_url_results`): `?password=` and its alias
/// `?pass=` replace the password, `?user=` replaces the user, and when
/// `?user=` is given but no password is, the user part of the URL becomes
/// the password (`mailtos://PASSWORD@host?user=admin`).
pub fn url_credentials(url: &ParsedUrl) -> (Option<String>, Option<String>) {
  let mut password = url.password.clone();
  if let Some(p) = url.get("password") {
    password = Some(p.to_string());
  }
  if let Some(p) = url.get("pass") {
    password = Some(p.to_string());
  }
  let mut user = url.user.clone();
  if let Some(u) = url.get("user") {
    if password.is_none() {
      password = url.user.clone();
    }
    user = Some(u.to_string());
  }
  (user, password)
}

/// Split URL path entries the way upstream `URLBase.split_path()` does: on
/// `/` (already done by `ParsedUrl`) and also on whitespace, commas and
/// backslashes. Empty entries are dropped.
pub fn split_path(parts: &[String]) -> Vec<String> {
  parts.iter().flat_map(|p| p.split(|c: char| c.is_whitespace() || c == ',' || c == '\\')).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Upstream `parse_list()`: split every entry on whitespace, commas,
/// semicolons and square brackets, then return the unique values sorted.
pub fn parse_list<S: AsRef<str>>(entries: &[S]) -> Vec<String> {
  let mut out: Vec<String> = entries
    .iter()
    .flat_map(|e| e.as_ref().split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')).map(str::to_string).collect::<Vec<_>>())
    .filter(|s| !s.is_empty())
    .collect();
  out.sort();
  out.dedup();
  out
}

/// Whether the authority of the raw URL carries a port that upstream
/// `parse_url()` rejects for host-verified URLs, such as `host:` or
/// `host:garbage`. `ParsedUrl` keeps those as part of the host instead.
pub fn has_invalid_port(raw: &str) -> bool {
  let Some((_, rest)) = raw.split_once("://") else { return false };
  let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
  let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
  let after_host = match host_port.strip_prefix('[') {
    Some(v6) => v6.split_once(']').map(|(_, r)| r).unwrap_or(""),
    None => host_port.find(':').map(|i| &host_port[i..]).unwrap_or(""),
  };
  match after_host.strip_prefix(':') {
    Some(port) => !matches!(port.parse::<u32>(), Ok(1..=65535)),
    None => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_bool_with_default() {
    for val in &["yes", "YES", "y", "true", "t", "on", "1", "enable", "always", "tr"] {
      assert!(parse_bool_with_default(val, false), "Expected true for '{}'", val);
    }
    for val in &["no", "none", "n", "false", "f", "0", "off", "never", "disable", "deny"] {
      assert!(!parse_bool_with_default(val, true), "Expected false for '{}'", val);
    }
    // Anything unrecognised returns the default
    for val in &["", "random", "01", "fx", "yx"] {
      assert!(parse_bool_with_default(val, true), "Expected default true for '{}'", val);
      assert!(!parse_bool_with_default(val, false), "Expected default false for '{}'", val);
    }
  }

  #[test]
  fn test_url_credentials() {
    let creds = |u: &str| url_credentials(&ParsedUrl::parse(u).unwrap());
    assert_eq!(creds("smpp://user:pass@host"), (Some("user".into()), Some("pass".into())));
    assert_eq!(creds("smpp://user:pass@host?password=pw"), (Some("user".into()), Some("pw".into())));
    assert_eq!(creds("smpp://user:pass@host?pass=pw&password=other"), (Some("user".into()), Some("pw".into())));
    assert_eq!(creds("smpp://host?user=u&password=pw"), (Some("u".into()), Some("pw".into())));
    // ?user= turns the URL's user into the password when none was given
    assert_eq!(creds("smpp://admin@host?user=bob"), (Some("bob".into()), Some("admin".into())));
    assert_eq!(creds("smpp://host"), (None, None));
  }

  #[test]
  fn test_split_path_and_parse_list() {
    let parts = vec!["a b".to_string(), "c,d\\e".to_string(), "".to_string()];
    assert_eq!(split_path(&parts), vec!["a", "b", "c", "d", "e"]);
    assert_eq!(parse_list(&["b, a", "c;[a] d", ""]), vec!["a", "b", "c", "d"]);
  }

  #[test]
  fn test_has_invalid_port() {
    assert!(!has_invalid_port("growl://localhost"));
    assert!(!has_invalid_port("growl://localhost:2000/path"));
    assert!(!has_invalid_port("xmpp://user:pass@localhost/target"));
    assert!(!has_invalid_port("xmpp://[::1]:5222"));
    assert!(has_invalid_port("growl://localhost:"));
    assert!(has_invalid_port("growl://localhost:garbage"));
    assert!(has_invalid_port("growl://localhost:0"));
    assert!(has_invalid_port("growl://localhost:70000"));
  }
}
