//! Kaleyra SMS notifications.
//!
//! ```text
//! kaleyra://{sid}:{apikey}@{sender}/{to_phone}/{to_phone}
//! klr://{sid}:{apikey}@{sender}/{to_phone}
//! ```
//!
//! Kaleyra hosts each account in one region: Singapore (`sg`, the default),
//! India (`in`) or Europe (`eu`), picked with `?region=`. Indian accounts
//! must name a registered DLT template with `?template={id}`.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};

/// The API host that serves each region.
const REGIONS: &[(&str, &str)] = &[("in", "https://api.in.kaleyra.io"), ("sg", "https://api.kaleyra.io"), ("eu", "https://api.eu.kaleyra.io")];

/// The route types Kaleyra accepts.
const MESSAGE_TYPES: &[&str] = &["DEFAULT", "TXN", "TXND", "MKT", "OTP"];

/// The most recipients Kaleyra accepts in one transactional request.
const BATCH_SIZE: usize = 100;

pub struct Kaleyra {
  sid: String,
  apikey: String,
  sender: String,
  /// The API base URL for the account's region
  api_base: &'static str,
  message_type: Option<String>,
  template: Option<String>,
  flash: bool,
  batch: bool,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Kaleyra {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream strips query values and ignores empty ones
    let qsd = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    let sid = qsd("sid").map(str::to_string).or_else(|| url.user.clone()).unwrap_or_default();
    let Some(sid) = validate(&sid) else {
      tracing::warn!("A Kaleyra Account SID must be specified.");
      return None;
    };
    let apikey = qsd("key").map(str::to_string).or_else(|| url.password.clone()).unwrap_or_default();
    let Some(apikey) = validate(&apikey) else {
      tracing::warn!("A Kaleyra API Key must be specified.");
      return None;
    };
    // ?sender= wins over ?from=, which wins over the host
    let sender = qsd("sender").or_else(|| qsd("from")).map(str::to_string).unwrap_or_else(|| decode(url.host.as_deref().unwrap_or_default()));
    let Some(sender) = validate(&sender) else {
      tracing::warn!("A Kaleyra Sender ID must be specified.");
      return None;
    };

    let region = qsd("region").map(str::to_lowercase).unwrap_or_else(|| "sg".to_string());
    let Some((_, api_base)) = REGIONS.iter().find(|(name, _)| *name == region) else {
      tracing::warn!("The Kaleyra region specified ({}) is invalid.", region);
      return None;
    };

    // The route type is optional; Kaleyra picks one when not given
    let message_type = match qsd("type").map(str::to_uppercase) {
      Some(t) if MESSAGE_TYPES.contains(&t.as_str()) => Some(t),
      Some(t) => {
        tracing::warn!("The Kaleyra message type specified ({}) is invalid.", t);
        return None;
      }
      None => None,
    };

    let template = match qsd("template") {
      Some(t) => Some(validate(t)?),
      None => None,
    };

    let flash = url.get("flash").map(parse_bool).unwrap_or(false);
    let batch = url.get("batch").map(parse_bool).unwrap_or(false);

    let mut entries = url.path_parts.clone();
    if let Some(to) = qsd("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for entry in parse_phone_no(&entries) {
      match is_phone_no(&entry, 10) {
        Some(no) => targets.push(no.full),
        None => tracing::warn!("Dropped invalid Kaleyra phone # ({}) specified.", entry),
      }
    }

    Some(Self { sid, apikey, sender, api_base, message_type, template, flash, batch, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Kaleyra",
      service_url: Some("https://www.kaleyra.com/"),
      setup_url: Some("https://appriseit.com/services/kaleyra/"),
      protocols: vec!["kaleyra", "klr"],
      description: "Send SMS via Kaleyra.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Kaleyra {
  fn schemas(&self) -> &[&str] {
    &["kaleyra", "klr"]
  }
  fn service_name(&self) -> &str {
    "Kaleyra"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // Kaleyra accepts up to 4000 characters and splits them into SMS parts
  fn body_maxlen(&self) -> usize {
    4000
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Kaleyra targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/v1/{}/messages", service_base(self.api_base), urlencoding::encode(&self.sid));

    // Kaleyra picks the right encoding for the text with AUTO
    let mut payload = json!({ "to": null, "sender": self.sender, "body": ctx.body, "unicode": "AUTO" });
    if let Some(message_type) = &self.message_type {
      payload["type"] = json!(message_type);
    }
    if let Some(template) = &self.template {
      payload["template_id"] = json!(template);
    }
    if self.flash {
      payload["flash"] = json!("1");
    }

    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    let mut ok = true;
    for targets in self.targets.chunks(batch_size) {
      // Several phone numbers are separated by a comma
      let to = targets.join(",");
      payload["to"] = json!(to);

      tracing::debug!("Kaleyra POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let resp = match client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("api-key", self.apikey.as_str())
        .json(&payload)
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Kaleyra notification to {}.", to);
          tracing::debug!("Socket Exception: {}", e);
          ok = false;
          continue;
        }
      };
      let status = resp.status().as_u16();
      let text = resp.text().await.unwrap_or_default();
      let content = serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).unwrap_or(Value::Null);

      // A successful answer carries an empty "error" object
      if !(200..300).contains(&status) || content.get("error").is_some_and(truthy) {
        let reason = content.get("message").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or_else(|| status_reason(status));
        tracing::warn!("Failed to send Kaleyra notification to {}: {}, error={}.", to, reason, status);
        tracing::debug!("Response Details: {}", truncate(&text));
        ok = false;
        continue;
      }
      tracing::info!("Sent Kaleyra notification to {}.", to);
    }
    Ok(ok)
  }
}

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
  match v {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::Number(n) => n.as_f64() != Some(0.0),
    Value::String(s) => !s.is_empty(),
    Value::Array(a) => !a.is_empty(),
    Value::Object(o) => !o.is_empty(),
  }
}

/// Upstream `validate_regex()` with its default pattern: the value must not
/// start with whitespace, and is returned stripped.
fn validate(value: &str) -> Option<String> {
  if value.is_empty() || value.starts_with(char::is_whitespace) { None } else { Some(value.trim().to_string()) }
}

fn status_reason(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Check the phone numbers, sender and message.",
    401 => "Unauthorized - Invalid SID or API Key, or a low balance.",
    429 => "Too many requests were made.",
    500 => "Kaleyra is unavailable, try again later.",
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
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const SID: &str = "HXAP16XXXXXX97IN";
  const APIKEY: &str = "Ac4b0123456789abcdef0123456789a21f";
  const SENDER: &str = "KLRHXA";

  fn parse(url: &str) -> Option<Kaleyra> {
    Kaleyra::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(202).set_body_json(json!({
      "id": "ee8a0000-0000-0000-0000-9b7170000030d",
      "sender": SENDER,
      "totalCount": 1,
      "data": [{"message_id": "ee8a0000-0000-0000-0000-9b7170000030d-0", "recipient": "15551234567"}],
      "error": {},
    }))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("kaleyra://{SID}:{APIKEY}@{SENDER}"),
      format!("kaleyra://{SID}:{APIKEY}@{SENDER}/15551234567"),
      format!("kaleyra://{SID}:{APIKEY}@{SENDER}/15551234567/915551234568?region=in&type=txn&template=1012345678901&flash=yes&batch=yes"),
      format!("kaleyra://?sid={SID}&key={APIKEY}&from={SENDER}&to=15551234567,15551234568"),
      format!("kaleyra://{SID}:{APIKEY}@/15551234567?sender={SENDER}"),
      format!("klr://{SID}:{APIKEY}@{SENDER}/15551234567"),
      format!("kaleyra://{SID}:{APIKEY}@{SENDER}/15551234567/123"),
      format!("kaleyra://{SID}:{APIKEY}@{SENDER}/123"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "kaleyra://".to_string(),
      format!("kaleyra://{SENDER}/15551234567"),
      format!("kaleyra://{SID}@{SENDER}/15551234567"),
      format!("kaleyra://{SID}:{APIKEY}@/15551234567"),
      format!("kaleyra://{SID}:{APIKEY}@{SENDER}/15551234567?region=us"),
      format!("kaleyra://{SID}:{APIKEY}@{SENDER}/15551234567?type=invalid"),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values checked against upstream
    let k = parse(&format!("kaleyra://{SID}:{APIKEY}@{SENDER}/15551234567")).unwrap();
    assert_eq!((k.sid.as_str(), k.apikey.as_str(), k.sender.as_str()), (SID, APIKEY, SENDER));
    assert_eq!(k.api_base, "https://api.kaleyra.io");
    assert_eq!((k.message_type.as_deref(), k.template.as_deref(), k.flash, k.batch), (None, None, false, false));
    assert_eq!(k.targets, vec!["15551234567"]);

    // ?sender= wins over ?from=; values are stripped and normalised
    let k = parse("kaleyra://?sid=S&key=K&from=F&sender=G&to=15551234567&type=%20otp%20&region=%20SG").unwrap();
    assert_eq!((k.sid.as_str(), k.apikey.as_str(), k.sender.as_str()), ("S", "K", "G"));
    assert_eq!(k.message_type.as_deref(), Some("OTP"));
    assert_eq!(k.api_base, "https://api.kaleyra.io");

    let k = parse("kaleyra://S:K@F/15551234567?flash=garbage&batch=1&template=%20T&region=EU").unwrap();
    assert_eq!((k.flash, k.batch, k.template.as_deref()), (false, true, Some("T")));
    assert_eq!(k.api_base, "https://api.eu.kaleyra.io");

    let k = parse("kaleyra://S:K@F/15551234567/123/abcd?region=in").unwrap();
    assert_eq!(k.api_base, "https://api.in.kaleyra.io");
    assert_eq!(k.targets, vec!["15551234567"]);
  }

  #[test]
  fn test_service_details() {
    let k = parse("klr://S:K@F/15551234567").unwrap();
    assert_eq!(k.body_maxlen(), 4000);
    assert_eq!(k.title_maxlen(), 0);
    assert!(!k.attachment_support());
    assert_eq!(Kaleyra::static_details().protocols, vec!["kaleyra", "klr"]);
  }

  #[tokio::test]
  async fn test_send_batch() {
    // Verified against upstream with pycapture.py
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v1/{SID}/messages")))
      .and(header("api-key", "APIKEY"))
      .and(header("Accept", "application/json"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({
        "to": "15551234567,915551234568",
        "sender": SENDER,
        "body": "B",
        "unicode": "AUTO",
        "type": "TXN",
        "template_id": "1012345678901",
        "flash": "1",
      })))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    let svc =
      from_url(&format!("kaleyra://{SID}:APIKEY@{SENDER}/15551234567/915551234568?region=in&type=txn&template=1012345678901&flash=yes&batch=yes")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_individual() {
    // One request per number; the SID is escaped in the path
    let server = MockServer::start().await;
    for to in ["15551234567", "15551234568"] {
      Mock::given(method("POST"))
        .and(path("/v1/S%2Fx/messages"))
        .and(body_json(json!({"to": to, "sender": SENDER, "body": "B", "unicode": "AUTO"})))
        .respond_with(ok_response())
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("klr://S%2Fx:K@{SENDER}/15551234567/15551234568?region=EU")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url(&format!("kaleyra://S:K@{SENDER}/15551234567")).unwrap();
    for (status, body) in [
      (200, r#"{"code": "E413", "message": "Invalid/incorrect inputs", "data": [], "error": {"sender": "Invalid sender"}}"#),
      (400, r#"{"message": "Bad"}"#),
      (401, ""),
      (500, "not json"),
      (999, ""),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status).set_body_string(body)).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "status {}", status);
    }

    // A 2xx answer that is not JSON relies on the status code
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("garbage")).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // One failed target fails the send, but the rest are still tried
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({"to": "15551234567", "sender": SENDER, "body": "B", "unicode": "AUTO"})))
      .respond_with(ResponseTemplate::new(400))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).respond_with(ok_response()).expect(1).mount(&server).await;
    let svc = from_url(&format!("kaleyra://S:K@{SENDER}/15551234567/15551234568")).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // Nothing to notify
    let svc = from_url(&format!("kaleyra://S:K@{SENDER}/123")).unwrap();
    assert!(!svc.send(&ctx("B")).await.unwrap());

    // Connection errors fail the send
    let svc = from_url(&format!("kaleyra://S:K@{SENDER}/15551234567")).unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
  }
}
