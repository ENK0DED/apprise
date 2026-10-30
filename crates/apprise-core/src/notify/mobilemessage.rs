//! Mobile Message (Australia) SMS notifications.
//!
//! ```text
//! mobilemessage://{user}:{password}@{sender}/{to_phone}/{to_phone}
//! mobilemsg://{user}:{password}@{sender}/{to_phone}
//! ```
//!
//! Only Australian mobiles are supported, written as 04xxxxxxxx,
//! 4xxxxxxxx or 614xxxxxxxx; other numbers are dropped. `?max_parts=`
//! limits how many SMS parts (and credits) each message may use.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::phone_no::{is_phone_no, parse_phone_no};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};

/// The service accepts up to 10,000 messages in a single request.
const BATCH_SIZE: usize = 10000;

/// Character limits for one GSM-7 or Unicode message part.
const GSM7_PART_SIZE: usize = 153;
const UCS2_PART_SIZE: usize = 67;

pub struct MobileMessage {
  user: String,
  password: String,
  source: String,
  targets: Vec<String>,
  unicode: bool,
  max_parts: u32,
  reference: Option<String>,
  batch: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl MobileMessage {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream strips query values and ignores empty ones
    let qsd = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    // Basic authentication requires both API credentials
    let (Some(user), Some(password)) = (url.user.clone().filter(|u| !u.is_empty()), url.password.clone().filter(|p| !p.is_empty())) else {
      tracing::warn!("A Mobile Message API username and password must be specified.");
      return None;
    };

    // With ?from= the host becomes a target
    let host = decode(url.host.as_deref().unwrap_or_default());
    let (source, mut entries) = match qsd("from") {
      Some(from) => {
        let mut entries = parse_phone_no(&[host.as_str()]);
        entries.extend(url.path_parts.iter().cloned());
        (from.to_string(), entries)
      }
      None => (host, url.path_parts.clone()),
    };
    if let Some(to) = qsd("to") {
      entries.extend(parse_phone_no(&[to]));
    }

    let Some(source) = parse_sender(&source) else {
      tracing::warn!("The Mobile Message Sender ID specified ({}) is invalid.", source);
      return None;
    };

    let unicode = qsd("unicode").map(parse_bool).unwrap_or(false);
    let batch = qsd("batch").map(parse_bool).unwrap_or(true);

    let max_parts = match qsd("max_parts") {
      None => 10,
      Some(v) => match v.parse::<i64>() {
        Ok(n) if (1..=99).contains(&n) => n as u32,
        _ => {
          tracing::warn!("The Mobile Message max_parts specified ({}) must be between 1 and 99.", v);
          return None;
        }
      },
    };

    let reference = qsd("ref").and_then(validate);

    let mut targets = Vec::new();
    for entry in parse_phone_no(&entries) {
      // Local numbers may leave off the leading zero
      let Some(no) = is_phone_no(&entry, 9) else {
        tracing::warn!("Dropped invalid Mobile Message phone # ({}).", entry);
        continue;
      };
      // Convert accepted numbers to the API's 614xxxxxxxx format
      let no = match no.full {
        local if local.starts_with('0') => format!("61{}", &local[1..]),
        short if short.len() == 9 => format!("61{}", short),
        full => full,
      };
      if !is_au_mobile(&no) {
        tracing::warn!("Dropped non-Australian Mobile Message phone # ({}).", entry);
        continue;
      }
      targets.push(no);
    }

    Some(Self { user, password, source, targets, unicode, max_parts, reference, batch, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Mobile Message",
      service_url: Some("https://mobilemessage.com.au/"),
      setup_url: Some("https://appriseit.com/services/mobilemessage/"),
      protocols: vec!["mobilemessage", "mobilemsg"],
      description: "Send SMS to Australian mobiles via Mobile Message.",
      attachment_support: false,
    }
  }

  /// Send one batch. Returns whether every recipient in it was accepted.
  async fn send_batch(&self, client: &reqwest::Client, url: &str, targets: &[String], body: &str) -> bool {
    let messages: Vec<Value> = targets
      .iter()
      .map(|to| {
        let mut entry = json!({ "to": to, "message": body, "sender": self.source });
        if let Some(reference) = &self.reference {
          entry["custom_ref"] = json!(reference);
        }
        entry
      })
      .collect();
    let payload = json!({ "messages": messages, "enable_unicode": self.unicode, "max_parts": self.max_parts });

    tracing::debug!("Mobile Message POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let resp = match client
      .post(url)
      .header("User-Agent", APP_ID)
      .header("Accept", "application/json")
      .header("Idempotency-Key", uuid4())
      .basic_auth(&self.user, Some(&self.password))
      .json(&payload)
      .send()
      .await
    {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Mobile Message notification to {} target(s).", targets.len());
        tracing::debug!("Socket Exception: {}", e);
        return false;
      }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
      tracing::warn!("Failed to send Mobile Message notification to {} target(s): {}, error={}.", targets.len(), status_reason(status), status);
      tracing::debug!("Response Details: {}", truncate(&text));
      return false;
    }

    // A processed batch comes back as "complete" with one result per message
    let content = serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).unwrap_or(Value::Null);
    let results = content.get("results").and_then(Value::as_array).filter(|r| r.len() == targets.len());
    let (Some(results), Some("complete")) = (results, content.get("status").and_then(Value::as_str)) else {
      tracing::warn!("Mobile Message did not report back on every one of the {} target(s) sent.", targets.len());
      tracing::debug!("Response Details: {}", truncate(&text));
      return false;
    };

    // A success means the service took the message, not that the handset
    // has it yet
    let mut accepted = 0;
    for (target, result) in targets.iter().zip(results) {
      if result.get("status").and_then(Value::as_str) == Some("success") {
        accepted += 1;
        continue;
      }
      let error = result.get("error").and_then(Value::as_str).unwrap_or("no reason was given");
      let status = result.get("status").and_then(Value::as_str).unwrap_or("unknown");
      tracing::warn!("Mobile Message did not accept {}: {} (status={}).", target, error, status);
    }
    tracing::info!("Sent Mobile Message notification to {} of {} target(s).", accepted, targets.len());
    accepted == targets.len()
  }
}

#[async_trait]
impl Notify for MobileMessage {
  fn schemas(&self) -> &[&str] {
    &["mobilemessage", "mobilemsg"]
  }
  fn service_name(&self) -> &str {
    "Mobile Message"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// The longest message the configured number of parts allows; Unicode
  /// text needs the smaller UCS-2 parts.
  fn body_maxlen(&self) -> usize {
    self.max_parts as usize * if self.unicode { UCS2_PART_SIZE } else { GSM7_PART_SIZE }
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no valid Mobile Message targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/v1/messages", service_base("https://api.mobilemessage.com.au"));
    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    let mut ok = true;
    for targets in self.targets.chunks(batch_size) {
      if !self.send_batch(&client, &url, targets, &ctx.body).await {
        ok = false;
      }
    }
    Ok(ok)
  }
}

/// Validate a Sender ID (`^\+?[a-z0-9][a-z0-9 ()_.-]{1,19}$`, ignoring
/// case). Numeric Sender IDs are sent as digits only.
fn parse_sender(source: &str) -> Option<String> {
  let rest = source.strip_prefix('+').unwrap_or(source);
  let mut chars = rest.chars();
  let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
  let tail: Vec<char> = chars.collect();
  if !first_ok || !(1..=19).contains(&tail.len()) || !tail.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '(' | ')' | '_' | '.' | '-')) {
    return None;
  }
  let source = source.trim();
  // ^\+?[0-9\s()-]+$ marks a phone number Sender ID
  let numeric = source.strip_prefix('+').unwrap_or(source);
  if !numeric.is_empty() && numeric.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '-')) {
    return Some(source.chars().filter(char::is_ascii_digit).collect());
  }
  Some(source.to_string())
}

/// `^614\d{8}$`: an Australian mobile in its international form.
fn is_au_mobile(no: &str) -> bool {
  no.len() == 11 && no.starts_with("614") && no.chars().all(|c| c.is_ascii_digit())
}

/// A random (version 4) UUID.
fn uuid4() -> String {
  let mut b: [u8; 16] = rand::random();
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  let h = hex::encode(b);
  format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Upstream `validate_regex()` with its default pattern: the value must not
/// start with whitespace, and is returned stripped.
fn validate(value: &str) -> Option<String> {
  if value.is_empty() || value.starts_with(char::is_whitespace) { None } else { Some(value.trim().to_string()) }
}

fn status_reason(status: u16) -> &'static str {
  match status {
    400 => "Malformed request or a field failed validation.",
    401 => "Invalid API credentials.",
    403 => "Insufficient credit or the operation is not allowed.",
    422 => "The Idempotency Key was re-used with a different payload.",
    429 => "Too many requests in flight; five is the maximum.",
    _ => reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or(""),
  }
}

fn decode(s: &str) -> String {
  urlencoding::decode(s).map(|d| d.into_owned()).unwrap_or_else(|_| s.to_string())
}

fn truncate(text: &str) -> &str {
  match text.char_indices().nth(2000) {
    Some((i, _)) => &text[..i],
    None => text,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, header_exists, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<MobileMessage> {
    MobileMessage::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  /// A complete answer accepting `count` messages.
  fn ok(count: usize) -> ResponseTemplate {
    let results: Vec<Value> = (0..count).map(|i| json!({"to": format!("6141234567{}", i), "message_id": format!("id-{}", i), "status": "success"})).collect();
    ResponseTemplate::new(200).set_body_json(json!({"status": "complete", "total_cost": count, "results": results}))
  }

  fn entry(to: &str, body: &str, sender: &str) -> Value {
    json!({"to": to, "message": body, "sender": sender})
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "mobilemessage://user:pass@ALERTS/",
      "mobilemessage://user:pass@ALERTS/0412345678",
      "mobilemsg://user:pass@ALERTS/0412345678",
      "mobilemessage://user:pass@+61 400 000 000/61412345678",
      "mobilemessage://user:pass@ALERTS/0412345678/412345679/61412345680",
      "mobilemessage://user:pass@ALERTS/0412345678/abcd/15551231234",
      "mobilemessage://user:pass@ALERTS/15551231234",
      "mobilemessage://user:pass@ALERTS/0412345678?unicode=yes&batch=no&max_parts=2&ref=nightly",
      "mobilemessage://user:pass@0412345678/?from=ALERTS",
      "mobilemessage://user:pass@ALERTS/?to=0412345678,0498765432",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "mobilemessage://",
      "mobilemessage://:@/",
      "mobilemessage://user@ALERTS/0412345678",
      "mobilemessage://user:pass@/0412345678",
      "mobilemessage://user:pass@%20/0412345678",
      "mobilemessage://user:pass@ALERTS/0412345678?max_parts=invalid",
      "mobilemessage://user:pass@ALERTS/0412345678?max_parts=0",
      "mobilemessage://user:pass@ALERTS/0412345678?max_parts=100",
      "mobilemessage://user:pass@A/0412345678",
      "mobilemessage://user:pass@ABCDEFGHIJKLMNOPQRSTU/0412345678",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values checked against upstream
    let m = parse("mobilemessage://user:pass@ALERTS/0412345678").unwrap();
    assert_eq!((m.user.as_str(), m.password.as_str(), m.source.as_str()), ("user", "pass", "ALERTS"));
    assert_eq!(m.targets, vec!["61412345678"]);
    assert_eq!((m.unicode, m.max_parts, m.reference.as_deref(), m.batch), (false, 10, None, true));

    // Numeric Sender IDs are digits only
    assert_eq!(parse("mobilemessage://user:pass@+61 400 000 000/61412345678").unwrap().source, "61400000000");
    assert_eq!(parse("mobilemessage://user:pass@My Shop/0412345678").unwrap().source, "My Shop");

    // Local and short forms are converted; other countries are dropped
    let m = parse("mobilemessage://user:pass@ALERTS/0412345678/412345679/61412345680/abcd/15551231234/0212345678").unwrap();
    assert_eq!(m.targets, vec!["61412345678", "61412345679", "61412345680"]);

    let m = parse("mobilemessage://user:pass@ALERTS/0412345678?unicode=yes&batch=no&max_parts=2&ref=nightly").unwrap();
    assert_eq!((m.unicode, m.max_parts, m.reference.as_deref(), m.batch), (true, 2, Some("nightly"), false));

    // ?from= makes the host a target
    let m = parse("mobilemessage://user:pass@0412345678?from=Unraid&to=61498765432").unwrap();
    assert_eq!(m.source, "Unraid");
    assert_eq!(m.targets, vec!["61412345678", "61498765432"]);

    // Empty values fall back to the defaults
    let m = parse("mobilemessage://user:pass@ALERTS/0412345678?batch=&unicode=&max_parts=&ref=").unwrap();
    assert_eq!((m.unicode, m.max_parts, m.reference.as_deref(), m.batch), (false, 10, None, true));
  }

  #[test]
  fn test_body_maxlen() {
    assert_eq!(parse("mobilemessage://user:pass@ALERTS/0412345678").unwrap().body_maxlen(), 1530);
    assert_eq!(parse("mobilemessage://user:pass@ALERTS/0412345678?unicode=yes").unwrap().body_maxlen(), 670);
    assert_eq!(parse("mobilemessage://user:pass@ALERTS/0412345678?max_parts=1").unwrap().body_maxlen(), 153);
    assert_eq!(parse("mobilemessage://user:pass@ALERTS/0412345678?max_parts=99&unicode=yes").unwrap().body_maxlen(), 6633);
    let m = parse("mobilemsg://user:pass@ALERTS/0412345678").unwrap();
    assert_eq!(m.title_maxlen(), 0);
    assert!(!m.attachment_support());
  }

  #[test]
  fn test_uuid4() {
    let a = uuid4();
    assert_eq!(a.len(), 36);
    assert_eq!(&a[14..15], "4");
    assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    assert_ne!(a, uuid4());
  }

  #[tokio::test]
  async fn test_send_batch() {
    // Matches the apprise-go goldens captured from upstream
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/messages"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .and(header("Accept", "application/json"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(header_exists("Idempotency-Key"))
      .and(body_json(json!({
        "messages": [
          {"to": "61412345678", "message": "Title\r\nBody", "sender": "Unraid", "custom_ref": "storage"},
          {"to": "61498765432", "message": "Title\r\nBody", "sender": "Unraid", "custom_ref": "storage"},
        ],
        "enable_unicode": false,
        "max_parts": 10,
      })))
      .respond_with(ok(2))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("mobilemessage://user:pass@Unraid/0412345678/61498765432?ref=storage").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Title\r\nBody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_individual() {
    let server = MockServer::start().await;
    for to in ["61412345678", "61498765432"] {
      Mock::given(method("POST"))
        .and(body_json(json!({"messages": [entry(to, "Title\r\n你好", "Unraid")], "enable_unicode": true, "max_parts": 2})))
        .respond_with(ok(1))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("mobilemsg://user:pass@Unraid/412345678/498765432?batch=no&unicode=yes&max_parts=2").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Title\r\n你好"))).await.unwrap());

    // Every request carries its own Idempotency-Key
    let requests = server.received_requests().await.unwrap();
    let keys: Vec<_> = requests.iter().map(|r| r.headers.get("idempotency-key").unwrap().to_str().unwrap().to_string()).collect();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
  }

  #[tokio::test]
  async fn test_send_results() {
    let svc = from_url("mobilemessage://user:pass@ALERTS/0412345678/0498765432").unwrap();
    let failures = [
      // One recipient refused
      json!({"status": "complete", "results": [{"status": "success"}, {"status": "error", "error": "Invalid number"}]}),
      // Not every recipient reported on
      json!({"status": "complete", "results": [{"status": "success"}]}),
      // Not complete
      json!({"status": "pending", "results": [{"status": "success"}, {"status": "success"}]}),
      // Unreadable entries
      json!({"status": "complete", "results": ["x", {"status": "success"}]}),
      json!({"status": "complete"}),
      json!([1, 2]),
    ];
    for body in failures {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(&body)).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "{}", body);
    }

    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("garbage")).expect(1).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ok(2)).expect(1).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url("mobilemessage://user:pass@ALERTS/0412345678").unwrap();
    for status in [201, 400, 401, 403, 422, 429, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "status {}", status);
    }

    // Nothing to notify
    let svc = from_url("mobilemessage://user:pass@ALERTS/15551231234").unwrap();
    assert!(!svc.send(&ctx("B")).await.unwrap());

    // Connection errors fail the send
    let svc = from_url("mobilemessage://user:pass@ALERTS/0412345678").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
  }
}
