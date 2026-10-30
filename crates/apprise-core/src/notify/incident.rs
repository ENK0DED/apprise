//! Behaviour the incident and on-call plugins share since upstream made them
//! consistent (PagerDuty, PagerTree, SIGNL4, ilert, incident.io, Squadcast,
//! GoAlert, Better Stack, Opsgenie, Jira, Splunk and Spike): partial action
//! names, alert keys that stay the same between notifications, keys that
//! survive a retry, the plain-text title of a Markdown body, and upstream's
//! wording for a failed request.

use crate::conversion::{commonmark_decode_backslash_escapes, pystr};
use crate::error::NotifyError;
use crate::utils::parse::{ParsedUrl, unquote};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};

/// The first of `choices` that starts with `value` (upstream's
/// `next(a for a in ACTIONS if a.startswith(value))`), so `r` picks
/// `resolve`. Callers trim or lower-case `value` where upstream does.
pub(super) fn match_prefix(choices: &[&'static str], value: &str) -> Option<&'static str> {
  choices.iter().copied().find(|c| c.starts_with(value))
}

/// [`match_prefix`] on the trimmed, lower-cased value, as most of the
/// family reads `?action=`.
pub(super) fn match_choice(choices: &[&'static str], value: &str) -> Option<&'static str> {
  match_prefix(choices, &pystr::strip(value).to_lowercase())
}

/// Upstream `^[A-Z0-9_-]+$` (case-insensitive), the shape of the
/// integration, webhook and API keys several of these services hand out.
pub(super) fn is_key(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The first line of `body`, stripped (`body.split("\n", 1)[0].strip()`).
pub(super) fn first_line(body: &str) -> &str {
  pystr::strip(body.split('\n').next().unwrap_or_default())
}

/// The SHA-256 hex digest of `text`.
pub(super) fn sha256_hex(text: &str) -> String {
  hex::encode(Sha256::digest(text.as_bytes()))
}

/// The key that names an alert between notifications: the SHA-256 of the
/// title, or of the first line of the body when there is none, so a later
/// success finds the alert a failure raised.
pub(super) fn title_key(title: &str, body: &str) -> String {
  sha256_hex(if title.is_empty() { first_line(body) } else { title })
}

/// A new random key for each notification, kept across its retries (upstream
/// `recall(name)` / `remember(name, uuid4())`) so a retry never raises a
/// second alert.
pub(super) fn retry_key(name: &str) -> String {
  if let Some(key) = crate::delivery::recall(name, false).and_then(|v| v.as_str().map(str::to_string)) {
    return key;
  }
  let key = super::apprise_api::uuid4();
  crate::delivery::remember(name, serde_json::json!(key), false);
  key
}

/// The plain-text title of a Markdown body: `title` when there is one, else
/// the body without the backslash escapes Apprise added when converting it,
/// cut to `maxlen` characters.
pub(super) fn plain_title(title: &str, body: &str, maxlen: usize) -> String {
  if !title.is_empty() {
    return title.to_string();
  }
  commonmark_decode_backslash_escapes(body).chars().take(maxlen).collect()
}

/// Upstream `unquote(results["qsd"][key])` when the argument is set and not
/// empty (`if key in results["qsd"] and len(results["qsd"][key])`).
pub(super) fn arg(url: &ParsedUrl, key: &str) -> Option<String> {
  url.get(key).filter(|v| !v.is_empty()).map(unquote)
}

/// Upstream `{unquote(k): unquote(v) for k, v in results["qsd+"].items()}`
/// (or `qsd:`/`qsd-`), in URL order.
pub(super) fn unquoted_map(map: &IndexMap<String, String>) -> Vec<(String, String)> {
  let mut out: Vec<(String, String)> = Vec::new();
  for (k, v) in map {
    let (k, v) = (unquote(k), unquote(v));
    match out.iter_mut().find(|(key, _)| *key == k) {
      Some(entry) => entry.1 = v,
      None => out.push((k, v)),
    }
  }
  out
}

/// A JSON object of `entries`.
pub(super) fn json_object(entries: &[(String, String)]) -> serde_json::Value {
  serde_json::Value::Object(entries.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect())
}

/// Upstream `http_response_code_lookup(code, response_mask)`: the plugin's
/// own description of `status`, else the shared `HTML_LOOKUP` one.
pub(super) fn status_text(status: u16, overrides: &[(u16, &'static str)]) -> &'static str {
  if let Some((_, text)) = overrides.iter().find(|(code, _)| *code == status) {
    return text;
  }
  match status {
    400 => "Bad Request - Unsupported Parameters.",
    401 => "Verification Failed.",
    404 => "Page not found.",
    405 => "Method not allowed.",
    500 => "Internal server error.",
    503 => "Servers are overloaded.",
    _ => "",
  }
}

/// `"{reason}, error={status}"`, the way upstream reports a failed request.
pub(super) fn status_detail(status: u16, overrides: &[(u16, &'static str)]) -> String {
  let reason = status_text(status, overrides);
  format!("{}{}error={}", reason, if reason.is_empty() { "" } else { ", " }, status)
}

/// What the service answered.
pub(super) struct Reply {
  pub status: u16,
  pub body: String,
  /// The status is one the plugin accepts.
  pub delivered: bool,
}

impl Reply {
  /// `Ok(true)` when delivered, else the service error.
  pub fn into_result(self) -> Result<bool, NotifyError> {
    if self.delivered { Ok(true) } else { Err(NotifyError::ServiceError { status: self.status, body: self.body }) }
  }
}

/// Throttle, send `request` and read the reply. A status in `accepted` is
/// logged as sent; any other is logged with upstream's wording for `what`
/// (`"PagerTree notification"`, say).
pub(super) async fn exchange(
  request: reqwest::RequestBuilder,
  what: &str,
  accepted: &[u16],
  overrides: &[(u16, &'static str)],
) -> Result<Reply, reqwest::Error> {
  crate::notify::throttle().await;
  let resp = request.send().await?;
  let status = resp.status().as_u16();
  let body = resp.text().await.unwrap_or_default();
  let delivered = accepted.contains(&status);
  if delivered {
    tracing::info!("Sent {}.", what);
  } else {
    tracing::warn!("Failed to send {}: {}.", what, status_detail(status, overrides));
  }
  Ok(Reply { status, body, delivered })
}

/// Log a connection error for one of several requests, which the plugin
/// counts as a failure and moves past.
pub(super) fn log_connection_error(what: &str, err: &reqwest::Error) {
  tracing::warn!("A Connection error occurred sending {}.", what);
  tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(err));
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_match_choice() {
    let actions = ["map", "trigger", "acknowledge", "resolve"];
    assert_eq!(match_choice(&actions, " R "), Some("resolve"));
    assert_eq!(match_choice(&actions, "ack"), Some("acknowledge"));
    assert_eq!(match_choice(&actions, "TRIG"), Some("trigger"));
    assert_eq!(match_choice(&actions, "invalid"), None);
    // Upstream matches the value as given unless it trims and lowers it
    assert_eq!(match_prefix(&actions, "R"), None);
  }

  #[test]
  fn test_title_key() {
    // The title, or the first line of the body when there is none
    assert_eq!(title_key("Disk full", "db01"), sha256_hex("Disk full"));
    assert_eq!(title_key("", "Disk full\nat 95%"), sha256_hex("Disk full"));
    assert_eq!(title_key("", "  Disk full \r\nat 95%"), sha256_hex("Disk full"));
    assert_ne!(title_key("", "one"), title_key("", "two"));
    assert_eq!(sha256_hex("Alert"), "44a57b22e03d9a661c22ffc5ffea51b3e1e33de24a53609e70f690dea8f70872");
  }

  #[test]
  fn test_plain_title() {
    assert_eq!(plain_title("T", "body", 250), "T");
    assert_eq!(plain_title("", "Server\\_1 is \\*down\\*", 250), "Server_1 is *down*");
    assert_eq!(plain_title("", &"x".repeat(300), 250), "x".repeat(250));
  }

  #[test]
  fn test_status_detail() {
    assert_eq!(status_detail(500, &[]), "Internal server error., error=500");
    assert_eq!(status_detail(999, &[]), "error=999");
    assert_eq!(status_detail(404, &[(404, "Not Found - Invalid Integration ID")]), "Not Found - Invalid Integration ID, error=404");
  }

  #[test]
  fn test_retry_key_without_retries() {
    // Nothing is remembered outside a retrying dispatch, so every call is new
    assert_ne!(retry_key("id"), retry_key("id"));
    assert!(crate::utils::plugin_args::is_uuid4(&retry_key("id")));
  }
}
