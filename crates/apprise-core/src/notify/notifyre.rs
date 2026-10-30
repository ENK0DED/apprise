//! Notifyre SMS and fax notifications.
//!
//! ```text
//! notifyre://{apikey}/{phone_no}/{phone_no}
//! notifyre://{apikey}/{fax_no}?mode=fax
//! ```
//!
//! SMS is the default mode. In fax mode the body is sent as a plain-text
//! first page, followed by each attachment as its own document.

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};

/// Notifyre REST API version.
const API_VERSION: &str = "20220711";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Sms,
  Fax,
}

pub struct Notifyre {
  apikey: String,
  mode: Mode,
  source: Option<String>,
  /// Defaults to the application ID
  campaign: Option<String>,
  template: String,
  reference: String,
  hq: bool,
  header: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Notifyre {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream strips query values and ignores empty ones
    let qsd = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    let apikey = url.host.clone().unwrap_or_default();
    if apikey.is_empty() {
      tracing::warn!("A Notifyre API key must be specified.");
      return None;
    }

    // Any start of a mode name selects it
    let mode = match qsd("mode").map(str::to_lowercase) {
      None => Mode::Sms,
      Some(m) if "sms".starts_with(m.as_str()) => Mode::Sms,
      Some(m) if "fax".starts_with(m.as_str()) => Mode::Fax,
      Some(m) => {
        tracing::warn!("The Notifyre mode specified ({}) is invalid.", m);
        return None;
      }
    };

    let source = match qsd("from") {
      Some(from) => match is_phone_no(from, 10) {
        Some(no) => Some(format!("+{}", no.full)),
        None => {
          tracing::warn!("The Notifyre source (From) phone # ({}) is invalid.", from);
          return None;
        }
      },
      None => None,
    };

    let mut entries = url.path_parts.clone();
    if let Some(to) = qsd("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for entry in parse_phone_no(&entries) {
      match is_phone_no(&entry, 10) {
        Some(no) => targets.push(format!("+{}", no.full)),
        None => tracing::warn!("Dropped invalid Notifyre phone # ({}) specified.", entry),
      }
    }

    Some(Self {
      apikey,
      mode,
      source,
      campaign: qsd("campaign").map(str::to_string),
      template: qsd("template").unwrap_or_default().to_string(),
      reference: qsd("ref").unwrap_or_default().to_string(),
      hq: qsd("hq").map(parse_bool).unwrap_or(true),
      header: qsd("header").unwrap_or_default().to_string(),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Notifyre",
      service_url: Some("https://notifyre.com/"),
      setup_url: Some("https://appriseit.com/services/notifyre/"),
      protocols: vec!["notifyre"],
      description: "Send SMS or faxes via Notifyre.",
      attachment_support: true,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Option<Value> {
    let campaign = self.campaign.as_deref().unwrap_or(ctx.asset.app_id.as_str());
    let source = self.source.as_deref().unwrap_or_default();
    match self.mode {
      Mode::Sms => {
        if !ctx.attachments.is_empty() {
          tracing::warn!("Notifyre SMS does not support attachments; use fax mode instead.");
        }
        let recipients: Vec<Value> = self.targets.iter().map(|t| json!({ "type": "mobile_number", "value": t })).collect();
        Some(json!({ "body": ctx.body, "recipients": recipients, "from": source, "campaignName": campaign }))
      }
      Mode::Fax => {
        // The body is always the first page
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut documents = Vec::new();
        if !ctx.body.is_empty() {
          documents.push(json!({ "base64Str": b64.encode(ctx.body.as_bytes()), "contentType": "text/plain" }));
        }
        for attachment in &ctx.attachments {
          documents.push(json!({ "base64Str": b64.encode(&attachment.data), "contentType": attachment.mime_type }));
        }
        if documents.is_empty() {
          tracing::warn!("Notifyre fax has no content to send.");
          return None;
        }
        let recipients: Vec<Value> = self.targets.iter().map(|t| json!({ "type": "fax_number", "value": t })).collect();
        Some(json!({
          "templateName": self.template,
          "recipients": recipients,
          "sendFrom": source,
          "isHighQuality": self.hq,
          "clientReference": self.reference,
          "documents": documents,
          "header": self.header,
          "subject": ctx.body,
          "campaignName": campaign,
          "scheduledDate": null,
        }))
      }
    }
  }
}

#[async_trait]
impl Notify for Notifyre {
  fn schemas(&self) -> &[&str] {
    &["notifyre"]
  }
  fn service_name(&self) -> &str {
    "Notifyre"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // Fax mode sends the attachments as document pages
  fn attachment_support(&self) -> bool {
    true
  }
  // An SMS segment holds 160 characters; a fax has no meaningful limit
  fn body_maxlen(&self) -> usize {
    match self.mode {
      Mode::Sms => 160,
      Mode::Fax => 32768,
    }
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("No Notifyre targets to notify.");
      return Ok(false);
    }
    let Some(payload) = self.payload(ctx) else {
      return Ok(false);
    };
    let (kind, endpoint) = match self.mode {
      Mode::Sms => ("SMS", "sms"),
      Mode::Fax => ("fax", "fax"),
    };
    let url = format!("{}/{}/{}/send", service_base("https://api.notifyre.com"), API_VERSION, endpoint);
    let client = build_client(self.verify_certificate)?;

    tracing::debug!("Notifyre {} POST URL: {} (cert_verify={})", kind, url, self.verify_certificate);
    crate::notify::throttle().await;
    let resp = match client.post(&url).header("User-Agent", APP_ID).header("x-api-token", self.apikey.as_str()).json(&payload).send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Notifyre {}.", kind);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return Ok(false);
      }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let content = serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).unwrap_or(Value::Null);

    if status != 200 {
      let reason = reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("");
      tracing::warn!("Failed to send Notifyre {}: {}, error={}.", kind, reason, status);
      tracing::debug!("Response Details: {}", truncate(&text));
      return Ok(false);
    }
    // The API can answer 200 with "success": false
    if content.get("success").is_some_and(|s| !truthy(s)) {
      let message = content.get("message").and_then(Value::as_str).unwrap_or("Unknown error");
      tracing::warn!("Notifyre {} failed: {}", kind, message);
      return Ok(false);
    }
    tracing::info!("Sent Notifyre {} notification.", kind);
    Ok(true)
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

fn truncate(text: &str) -> &str {
  match text.char_indices().nth(2000) {
    Some((i, _)) => &text[..i],
    None => text,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const API_KEY: &str = "abc123def456ghi789jkl0";

  fn parse(url: &str) -> Option<Notifyre> {
    Notifyre::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"success": true, "statusCode": 200, "message": "OK", "payload": {}}))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("notifyre://{API_KEY}"),
      format!("notifyre://{API_KEY}/+15551234567"),
      format!("notifyre://{API_KEY}/+15551234567/+15559876543"),
      format!("notifyre://{API_KEY}/+15551234567?mode=sms"),
      format!("notifyre://{API_KEY}/+15551234567?mode=fax"),
      format!("notifyre://{API_KEY}/+15551234567?mode=fax&from=+15559876543"),
      format!("notifyre://{API_KEY}/+15551234567?campaign=MyCampaign"),
      format!("notifyre://{API_KEY}/+15551234567?mode=fax&template=MyTpl"),
      format!("notifyre://{API_KEY}/+15551234567?mode=fax&hq=no&ref=CR123&header=CONF"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "notifyre://".to_string(),
      "notifyre://:@/".to_string(),
      format!("notifyre://{API_KEY}/+15551234567?mode=invalid"),
      format!("notifyre://{API_KEY}/+15551234567?from=123"),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values checked against upstream
    let n = parse("notifyre://KEY/+15551234567").unwrap();
    assert_eq!((n.apikey.as_str(), n.mode, n.source.as_deref(), n.campaign.as_deref()), ("KEY", Mode::Sms, None, None));
    assert_eq!((n.template.as_str(), n.reference.as_str(), n.hq, n.header.as_str()), ("", "", true, ""));
    assert_eq!(n.targets, vec!["+15551234567"]);

    assert_eq!(parse("notifyre://KEY/+15551234567?mode=F").unwrap().mode, Mode::Fax);
    assert_eq!(parse("notifyre://KEY/+15551234567?mode=").unwrap().mode, Mode::Sms);

    let n = parse("notifyre://KEY/+15551234567?mode=s&from=%2B1 555 000 0000&campaign=%20&hq=garbage").unwrap();
    assert_eq!((n.mode, n.source.as_deref(), n.campaign.as_deref(), n.hq), (Mode::Sms, Some("+15550000000"), None, false));

    let n = parse("notifyre://KEY/+15551234567?mode=fax&template=%20T%20&ref=R&header=H").unwrap();
    assert_eq!((n.template.as_str(), n.reference.as_str(), n.header.as_str()), ("T", "R", "H"));

    // Invalid numbers are dropped; ?to= adds more
    let n = parse("notifyre://KEY/123/abcd/15551234567?to=15559876543").unwrap();
    assert_eq!(n.targets, vec!["+15551234567", "+15559876543"]);
  }

  #[test]
  fn test_body_maxlen() {
    assert_eq!(parse("notifyre://KEY/+15551234567").unwrap().body_maxlen(), 160);
    assert_eq!(parse("notifyre://KEY/+15551234567?mode=fax").unwrap().body_maxlen(), 32768);
    let n = parse("notifyre://KEY/+15551234567").unwrap();
    assert_eq!(n.title_maxlen(), 0);
    assert!(n.attachment_support());
  }

  #[tokio::test]
  async fn test_send_sms() {
    // Matches the apprise-go goldens captured from upstream
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/20220711/sms/send"))
      .and(header("x-api-token", "apikey123"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({
        "body": "body",
        "recipients": [{"type": "mobile_number", "value": "+15551234567"}, {"type": "mobile_number", "value": "+15559876543"}],
        "from": "+15550000000",
        "campaignName": "MyCampaign",
      })))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("notifyre://apikey123/15551234567/15559876543?campaign=MyCampaign&from=15550000000").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());

    // The campaign defaults to the application ID; attachments are dropped
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/{API_VERSION}/sms/send")))
      .and(body_json(json!({
        "body": "title\r\nbody",
        "recipients": [{"type": "mobile_number", "value": "+15551234567"}],
        "from": "",
        "campaignName": "Apprise",
      })))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("notifyre://apikey123/15551234567").unwrap();
    let ctx = NotifyContext {
      body: "title\r\nbody".into(),
      attachments: vec![Attachment { name: "pixel.png".into(), data: vec![1, 2, 3], mime_type: "image/png".into() }],
      ..Default::default()
    };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_fax() {
    // Matches the apprise-go goldens captured from upstream
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/20220711/fax/send"))
      .and(header("x-api-token", "apikey123"))
      .and(body_json(json!({
        "templateName": "MyTemplate",
        "recipients": [{"type": "fax_number", "value": "+15551234567"}],
        "sendFrom": "",
        "isHighQuality": false,
        "clientReference": "Ref1",
        "documents": [
          {"base64Str": "dGl0bGUNCmJvZHk=", "contentType": "text/plain"},
          {"base64Str": "AQID", "contentType": "image/png"},
          {"base64Str": "cGxhaW4gdGV4dCBhdHRhY2htZW50Cg==", "contentType": "text/plain"},
        ],
        "header": "Confidential",
        "subject": "title\r\nbody",
        "campaignName": "Apprise",
        "scheduledDate": null,
      })))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("notifyre://apikey123/15551234567?mode=fax&ref=Ref1&header=Confidential&hq=no&template=MyTemplate").unwrap();
    let ctx = NotifyContext {
      body: "title\r\nbody".into(),
      attachments: vec![
        Attachment { name: "pixel.png".into(), data: vec![1, 2, 3], mime_type: "image/png".into() },
        Attachment { name: "notes.txt".into(), data: b"plain text attachment\n".to_vec(), mime_type: "text/plain".into() },
      ],
      ..Default::default()
    };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Defaults, without attachments
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/{API_VERSION}/fax/send")))
      .and(body_json(json!({
        "templateName": "",
        "recipients": [{"type": "fax_number", "value": "+15551234567"}],
        "sendFrom": "",
        "isHighQuality": true,
        "clientReference": "",
        "documents": [{"base64Str": "dGl0bGUNCmJvZHk=", "contentType": "text/plain"}],
        "header": "",
        "subject": "title\r\nbody",
        "campaignName": "Apprise",
        "scheduledDate": null,
      })))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("notifyre://apikey123/15551234567?mode=fax").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&self::ctx("title\r\nbody"))).await.unwrap());

    // A fax needs something to send
    assert!(!svc.send(&self::ctx("")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for mode in ["sms", "fax"] {
      let svc = from_url(&format!("notifyre://KEY/15551234567?mode={}", mode)).unwrap();
      for (status, body) in [(200, r#"{"success": false, "message": "Insufficient credit"}"#), (200, r#"{"success": null}"#), (201, ""), (500, ""), (999, "")] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
          .and(path(format!("/{API_VERSION}/{mode}/send")))
          .respond_with(ResponseTemplate::new(status).set_body_string(body))
          .expect(1)
          .mount(&server)
          .await;
        assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "{} status {} {}", mode, status, body);
      }

      // Without a success flag the status code decides
      for body in ["garbage", "[]", "{}"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
          .and(path(format!("/{API_VERSION}/{mode}/send")))
          .respond_with(ResponseTemplate::new(200).set_body_string(body))
          .expect(1)
          .mount(&server)
          .await;
        assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "{} {}", mode, body);
      }

      // Connection errors fail the send
      assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
    }

    // Nothing to notify
    let svc = from_url("notifyre://KEY").unwrap();
    assert!(!svc.send(&ctx("B")).await.unwrap());
  }
}
