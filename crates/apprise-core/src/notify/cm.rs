//! CM.com SMS (and WhatsApp, Viber, RCS) notifications.
//!
//! ```text
//! cm://{token}/{to_phone}
//! cm://{sender}@{token}/{to_phone}
//! ```
//!
//! The sender may be a name of up to 11 letters and digits or a phone
//! number of up to 16 digits; `Apprise` is used without one. `?channels=`
//! picks the channels CM.com may deliver over (sms by default).

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};

/// The channels that can reach a phone number, with the spelling the API
/// expects.
const CHANNELS: &[(&str, &str)] = &[("sms", "SMS"), ("whatsapp", "WhatsApp"), ("viber", "Viber"), ("rcs", "RCS")];

/// The most SMS parts CM.com recommends joining into one message.
const MAX_PARTS: u32 = 8;

/// The most recipients CM.com recommends placing in one request.
const BATCH_SIZE: usize = 1000;

pub struct Cm {
  token: String,
  source: Option<String>,
  channels: Vec<&'static str>,
  targets: Vec<String>,
  batch: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Cm {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The Product Token is the host, or ?token=
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => t.to_string(),
      None => decode(url.host.as_deref().unwrap_or_default()),
    };
    if !is_product_token(&token) {
      tracing::warn!("An invalid CM.com Product Token was specified.");
      return None;
    }

    // The optional sender sits before the token, or in ?from=
    let source = url.get("from").filter(|s| !s.is_empty()).map(str::to_string).or_else(|| url.user.clone());
    let source = source.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let batch = url.get("batch").filter(|b| !b.is_empty()).map(parse_bool).unwrap_or(false);

    // Upstream parse_list() de-duplicates and sorts the channels before
    // they are matched without regard to case.
    let mut requested: Vec<&str> = url
      .get("channels")
      .map(|c| c.split(|ch: char| ch.is_whitespace() || matches!(ch, '[' | ']' | ';' | ',')).filter(|s| !s.is_empty()).collect())
      .unwrap_or_default();
    requested.sort_unstable();
    requested.dedup();
    if requested.is_empty() {
      requested.push("sms");
    }
    let mut channels = Vec::new();
    for channel in requested {
      let channel = channel.to_lowercase();
      let Some((key, _)) = CHANNELS.iter().find(|(key, _)| *key == channel) else {
        tracing::warn!("An invalid CM.com channel ({}) was specified.", channel);
        return None;
      };
      if !channels.contains(key) {
        channels.push(*key);
      }
    }

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for entry in parse_phone_no(&entries) {
      match is_phone_no(&entry, 10).map(|p| p.full) {
        Some(no) => targets.push(no),
        None => tracing::warn!("Dropped invalid CM.com phone # ({}) specified.", entry),
      }
    }

    Some(Self { token, source, channels, targets, batch, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "CM.com",
      service_url: Some("https://www.cm.com/"),
      setup_url: Some("https://appriseit.com/services/cm/"),
      protocols: vec!["cm"],
      description: "Send SMS, WhatsApp, Viber or RCS messages via CM.com.",
      attachment_support: false,
    }
  }

  /// Send one batch. Returns whether every recipient in it was accepted.
  async fn send_batch(&self, client: &reqwest::Client, targets: &[String], body: &str, sender: &str) -> bool {
    // CM.com wants each number as 00 followed by the country code
    let recipients: Vec<(String, &String)> = targets.iter().map(|t| (format!("00{}", t), t)).collect();
    let channels: Vec<&str> = self.channels.iter().map(|c| CHANNELS.iter().find(|(key, _)| key == c).map(|(_, name)| *name).unwrap_or("SMS")).collect();
    let payload = json!({
      "messages": {
        "msg": [{
          "allowedChannels": channels,
          "from": sender,
          "to": recipients.iter().map(|(no, _)| json!({ "number": no })).collect::<Vec<_>>(),
          "body": { "type": "auto", "content": body },
          "minimumNumberOfMessageParts": 1,
          "maximumNumberOfMessageParts": MAX_PARTS,
        }]
      }
    });

    let url = format!("{}/v1.0/message", service_base("https://gw.messaging.cm.com"));
    tracing::debug!("CM.com POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let resp = match client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("X-CM-PRODUCTTOKEN", self.token.as_str())
      .header("Accept", "application/json")
      .json(&payload)
      .send()
      .await
    {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending CM.com notification to {} target(s).", targets.len());
        tracing::debug!("Socket Exception: {}", e);
        return false;
      }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let content: Value = serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));

    if status != 200 {
      // The whole request was refused
      let reason = content.get("details").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or_else(|| status_reason(status));
      tracing::warn!("Failed to send CM.com notification to {} target(s): {}, error={}.", targets.len(), reason, status);
      tracing::debug!("Response Details: {}", truncate(&text));
      return false;
    }

    // An errorCode of 0 means every recipient was taken
    if !content.get("errorCode").is_some_and(truthy) {
      tracing::info!("Sent CM.com notification to {} target(s).", targets.len());
      return true;
    }

    // Some recipients were refused, so look at each one. A recipient missing
    // from the response confirms nothing and counts as a refusal.
    let results: Vec<&Value> = content.get("messages").and_then(Value::as_array).map(|m| m.iter().filter(|m| m.is_object()).collect()).unwrap_or_default();
    let mut ok = true;
    for (no, target) in &recipients {
      let result = results.iter().rev().find(|m| m.get("to").and_then(Value::as_str) == Some(no.as_str()));
      if result.and_then(|r| r.get("messageErrorCode")).is_some_and(is_zero) {
        tracing::info!("Sent CM.com notification to {}.", target);
        continue;
      }
      let reason = result
        .and_then(|r| r.get("messageDetails"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| content.get("details").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("no reason was given");
      tracing::warn!("CM.com did not accept {}: {}", target, reason);
      ok = false;
    }
    ok
  }
}

#[async_trait]
impl Notify for Cm {
  fn schemas(&self) -> &[&str] {
    &["cm"]
  }
  fn service_name(&self) -> &str {
    "CM.com"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // Eight joined SMS parts of 153 characters each
  fn body_maxlen(&self) -> usize {
    1224
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no CM.com targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let sender = self.source.as_deref().unwrap_or(ctx.asset.app_id.as_str());
    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    let mut ok = true;
    for targets in self.targets.chunks(batch_size) {
      if !self.send_batch(&client, targets, &ctx.body, sender).await {
        ok = false;
      }
    }
    Ok(ok)
  }
}

/// A Product Token is a UUID: `^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$`
/// matched without regard to case.
fn is_product_token(token: &str) -> bool {
  let groups: Vec<&str> = token.split('-').collect();
  groups.len() == 5 && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, len)| g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit()))
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

/// Whether a JSON value equals 0 the way Python's `== 0` does.
fn is_zero(v: &Value) -> bool {
  matches!(v, Value::Bool(false)) || v.as_f64() == Some(0.0)
}

fn status_reason(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Check the product token and phone numbers.",
    405 => "Method Not Allowed.",
    500 => "Unknown Error.",
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

  const TOKEN: &str = "0123abcd-4567-89ef-abcd-0123456789ab";

  fn parse(url: &str) -> Option<Cm> {
    Cm::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"details": "Created 1 message(s)", "errorCode": 0, "messages": []}))
  }

  fn message(channels: &[&str], from: &str, to: &[&str], body: &str) -> Value {
    json!({
      "messages": {"msg": [{
        "allowedChannels": channels,
        "from": from,
        "to": to.iter().map(|n| json!({"number": n})).collect::<Vec<_>>(),
        "body": {"type": "auto", "content": body},
        "minimumNumberOfMessageParts": 1,
        "maximumNumberOfMessageParts": 8,
      }]}
    })
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("cm://{TOKEN}"),
      format!("cm://{TOKEN}/15551234567"),
      format!("cm://Alerts@{TOKEN}/15551234567/15551234568"),
      format!("cm://_/?token={TOKEN}&from=Alerts&to=15551234567,15551234568"),
      format!("cm://{TOKEN}/15551234567?channels=WhatsApp,sms&batch=yes"),
      format!("cm://{TOKEN}/15551234567/123"),
      format!("cm://{TOKEN}/123"),
      format!("cm://{}/15551234567", TOKEN.to_uppercase()),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in
      ["cm://".to_string(), "cm://abcd/15551234567".to_string(), format!("cm://{TOKEN}/15551234567?channels=pigeon"), format!("cm://{TOKEN}x/15551234567")]
    {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let cm = parse(&format!("cm://{TOKEN}/15551234567")).unwrap();
    assert_eq!(cm.token, TOKEN);
    assert_eq!(cm.source, None);
    assert_eq!(cm.channels, vec!["sms"]);
    assert_eq!(cm.targets, vec!["15551234567"]);
    assert!(!cm.batch);

    let cm = parse(&format!("cm://_/?token={TOKEN}&from=Alerts&to=15551234567,15551234568")).unwrap();
    assert_eq!(cm.source.as_deref(), Some("Alerts"));
    assert_eq!(cm.targets, vec!["15551234567", "15551234568"]);

    // Channels are de-duplicated and sorted the way upstream does
    let cm = parse(&format!("cm://{TOKEN}/15551234567?channels=WhatsApp,sms,SMS,viber&batch=yes")).unwrap();
    assert_eq!(cm.channels, vec!["sms", "whatsapp", "viber"]);
    assert!(cm.batch);
    let cm = parse(&format!("cm://{TOKEN}/15551234567?channels=WhatsApp,sms")).unwrap();
    assert_eq!(cm.channels, vec!["whatsapp", "sms"]);

    // Invalid phone numbers are dropped
    let cm = parse(&format!("cm://{TOKEN}/15551234567/123/abcd")).unwrap();
    assert_eq!(cm.targets, vec!["15551234567"]);
  }

  #[test]
  fn test_service_details() {
    let cm = parse(&format!("cm://{TOKEN}/15551234567")).unwrap();
    assert_eq!(cm.body_maxlen(), 1224);
    assert_eq!(cm.title_maxlen(), 0);
    assert!(!cm.attachment_support());
    assert_eq!(Cm::static_details().protocols, vec!["cm"]);
  }

  #[tokio::test]
  async fn test_send_payload() {
    // Verified against upstream with pycapture.py
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1.0/message"))
      .and(header("X-CM-PRODUCTTOKEN", TOKEN))
      .and(header("Content-Type", "application/json"))
      .and(header("Accept", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(message(&["SMS"], "Apprise", &["0015551234567"], "B")))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_json(message(&["SMS"], "Apprise", &["0015551234568"], "B")))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("cm://{TOKEN}/15551234567/+1 555 123 4568")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    // Upstream sorts the channels before lowercasing them, so WhatsApp
    // comes first here (verified with pycapture.py)
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(message(&["WhatsApp", "SMS"], "Alerts", &["0015551234567", "0015551234568"], "B")))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("cm://Alerts@{TOKEN}/15551234567/15551234568?channels=WhatsApp,sms&batch=yes")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_partial_rejection() {
    // A non-zero errorCode means each recipient has to be checked
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "details": "Created 1 message(s)",
        "errorCode": 201,
        "messages": [
          {"to": "0015551234567", "status": "Rejected", "messageDetails": "Not a number", "messageErrorCode": 303},
          {"to": "0015551234568", "status": "Accepted", "messageDetails": null, "messageErrorCode": 0},
        ],
      })))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("cm://{TOKEN}/15551234567/15551234568?batch=yes")).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // Every recipient accepted, even though errorCode is set
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "errorCode": 201,
        "messages": [
          {"to": "0015551234567", "messageErrorCode": 0},
          {"to": "0015551234568", "messageErrorCode": 0},
        ],
      })))
      .mount(&server)
      .await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // A recipient missing from the response counts as refused
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"errorCode": 201, "messages": [{"to": "0015551234567", "messageErrorCode": 0}]})))
      .mount(&server)
      .await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url(&format!("cm://{TOKEN}/15551234567")).unwrap();
    for (status, body) in [(400, r#"{"details": "Bad token"}"#), (500, "not json"), (999, "")] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status).set_body_string(body)).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "status {}", status);
    }

    // Unreadable 200 responses rely on the status code
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("garbage")).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // Nothing to notify
    let svc = from_url(&format!("cm://{TOKEN}/123")).unwrap();
    assert!(!svc.send(&ctx("B")).await.unwrap());

    // Connection errors fail the send
    let svc = from_url(&format!("cm://{TOKEN}/15551234567")).unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
  }
}
