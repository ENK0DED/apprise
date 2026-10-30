//! 800.com SMS and MMS notifications.
//!
//! ```text
//! eight00com://{token}@{from_phone}
//! eight00com://{token}@{from_phone}/{to_phone}/{to_phone}
//! ```
//!
//! Without a target the message goes to the sending number itself.
//! Attachments are sent as MMS.

use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::phone_no::{is_phone_no, parse_phone_no};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;

pub struct Eight00com {
  token: String,
  source: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Eight00com {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = decode(url.host.as_deref().unwrap_or_default());

    // With ?from= the host becomes another target
    let (source, mut entries) = match url.get("from").filter(|f| !f.is_empty()) {
      Some(from) => {
        let mut entries = parse_phone_no(&[host.as_str()]);
        entries.extend(url.path_parts.iter().cloned());
        (from.to_string(), entries)
      }
      None => (host, url.path_parts.clone()),
    };
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_phone_no(&[to]));
    }

    // The token is the user, or ?token=
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => t.to_string(),
      None => url.user.clone().unwrap_or_default(),
    };
    let Some(token) = validate(&token) else {
      tracing::warn!("An 800.com Personal Access Token must be specified.");
      return None;
    };

    let Some(source) = is_phone_no(&source, 10).map(|p| p.full) else {
      tracing::warn!("The 800.com from phone # ({}) is invalid.", source);
      return None;
    };

    let mut targets = Vec::new();
    let mut has_error = false;
    for entry in parse_phone_no(&entries) {
      match is_phone_no(&entry, 10).map(|p| p.full) {
        Some(no) => targets.push(no),
        None => {
          has_error = true;
          tracing::warn!("Dropped invalid 800.com phone # ({}).", entry);
        }
      }
    }
    if entries.is_empty() && !has_error {
      // Send to ourselves when no target is provided
      targets.push(source.clone());
    }

    Some(Self { token, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "800.com",
      service_url: Some("https://www.800.com"),
      setup_url: Some("https://appriseit.com/services/eight00com/"),
      protocols: vec!["eight00com"],
      description: "Send SMS and MMS messages via 800.com.",
      attachment_support: true,
    }
  }

  /// Send one message; returns whether 800.com accepted it.
  async fn send_one(&self, client: &reqwest::Client, recipient: &str, body: &str, attachments: &[Attachment]) -> bool {
    let sender = format!("+{}", self.source);
    let url = format!("{}/message", service_base("https://api.800.com"));
    tracing::debug!("800.com POST URL: {} (cert_verify={})", url, self.verify_certificate);

    let kind = if attachments.is_empty() { "SMS" } else { "MMS" };
    let req = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.token));
    let req = if attachments.is_empty() {
      req.json(&json!({ "sender": sender, "recipient": recipient, "message": body }))
    } else {
      let mut form = reqwest::multipart::Form::new().text("sender", sender).text("recipient", recipient.to_string()).text("message", body.to_string());
      for attachment in attachments {
        let part = reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(attachment.name.clone());
        let part = match part.mime_str(&attachment.mime_type) {
          Ok(part) => part,
          Err(_) => reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(attachment.name.clone()),
        };
        form = form.part("media[]", part);
      }
      req.multipart(form)
    };

    match req.send().await {
      Ok(resp) if resp.status().as_u16() == 200 => {
        tracing::info!("Sent 800.com {} to {}.", kind, recipient);
        true
      }
      Ok(resp) => {
        let status = resp.status().as_u16();
        tracing::warn!("Failed to send 800.com {} to {}: {}, error={}.", kind, recipient, status_reason(status), status);
        tracing::debug!("Response Details: {}", truncate(&resp.text().await.unwrap_or_default()));
        false
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending 800.com {} to {}.", kind, recipient);
        tracing::debug!("Socket Exception: {}", e);
        false
      }
    }
  }
}

#[async_trait]
impl Notify for Eight00com {
  fn schemas(&self) -> &[&str] {
    &["eight00com"]
  }
  fn service_name(&self) -> &str {
    "800.com"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  // 800.com supports long-form texts of up to 600 characters
  fn body_maxlen(&self) -> usize {
    600
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("No 800.com targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let mut ok = true;
    for target in &self.targets {
      if !self.send_one(&client, &format!("+{}", target), &ctx.body, &ctx.attachments).await {
        ok = false;
      }
    }
    Ok(ok)
  }
}

/// Upstream `validate_regex()` with its default pattern: the value must not
/// start with whitespace, and is returned stripped.
fn validate(value: &str) -> Option<String> {
  if value.is_empty() || value.starts_with(char::is_whitespace) { None } else { Some(value.trim().to_string()) }
}

fn status_reason(status: u16) -> &'static str {
  match status {
    401 => "Unauthorized - Invalid or missing Bearer Token.",
    422 => "Validation Error - Check sender/recipient number format.",
    429 => "Too many requests - Rate limit exceeded.",
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

  fn parse(url: &str) -> Option<Eight00com> {
    Eight00com::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let (t, f, a, b) = ("t".repeat(10), "8".repeat(10), "5".repeat(11), "6".repeat(11));
    for url in [
      "eight00com://GOODTOKEN@9876543210/12".to_string(),
      format!("eight00com://{t}@{f}"),
      format!("eight00com://{t}@{f}/{a}"),
      format!("eight00com://{t}@{f}/{a}/{b}"),
      format!("eight00com://?token={t}&from={f}"),
      format!("eight00com://?token={t}&from={f}&to={a}"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["eight00com://", "eight00com://:@/", "eight00com://GOODTOKEN@badphone", "eight00com://GOODTOKEN@123"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // No target sends to ourselves
    let e = parse("eight00com://token123@8005551234").unwrap();
    assert_eq!(e.token, "token123");
    assert_eq!(e.source, "8005551234");
    assert_eq!(e.targets, vec!["8005551234"]);

    let e = parse("eight00com://token123@+1 (800) 555-1234/15551234567/+1-555-123-4568").unwrap();
    assert_eq!(e.source, "18005551234");
    assert_eq!(e.targets, vec!["15551234567", "15551234568"]);

    // ?from= makes the host a target
    let e = parse("eight00com://token123@15551234567/15559876543?from=15550000000").unwrap();
    assert_eq!(e.source, "15550000000");
    assert_eq!(e.targets, vec!["15551234567", "15559876543"]);

    // ?token= and ?to=
    let e = parse("eight00com://?token=abc&from=8005551234&to=15551234567,15551234568").unwrap();
    assert_eq!(e.token, "abc");
    assert_eq!(e.targets, vec!["15551234567", "15551234568"]);

    // Only invalid targets leaves nothing to notify
    let e = parse("eight00com://token123@9876543210/12").unwrap();
    assert!(e.targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let e = parse("eight00com://token123@8005551234").unwrap();
    assert_eq!(e.body_maxlen(), 600);
    assert_eq!(e.title_maxlen(), 0);
    assert!(e.attachment_support());
    assert_eq!(Eight00com::static_details().protocols, vec!["eight00com"]);
  }

  #[tokio::test]
  async fn test_send_sms() {
    // Matches the apprise-go goldens captured from upstream
    let server = MockServer::start().await;
    for recipient in ["+15551234567", "+15559876543"] {
      Mock::given(method("POST"))
        .and(path("/message"))
        .and(header("Authorization", "Bearer token123"))
        .and(header("Content-Type", "application/json"))
        .and(header("User-Agent", APP_ID))
        .and(body_json(json!({"sender": "+15550000000", "recipient": recipient, "message": "body"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("eight00com://token123@15551234567/15559876543?from=15550000000").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_mms() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/message"))
      .and(header("Authorization", "Bearer token123"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("eight00com://token123@15550000000/15551234567").unwrap();
    let ctx = NotifyContext {
      body: "title\r\nbody".into(),
      attachments: vec![
        Attachment { name: "pixel.png".into(), data: b"\x89PNG".to_vec(), mime_type: "image/png".into() },
        Attachment { name: "notes.txt".into(), data: b"notes".to_vec(), mime_type: "text/plain".into() },
      ],
      ..Default::default()
    };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let content_type = requests[0].headers.get("content-type").unwrap().to_str().unwrap();
    assert!(content_type.starts_with("multipart/form-data; boundary="));
    let body = String::from_utf8_lossy(&requests[0].body);
    // Form fields first, then each file as media[]
    let sender = body.find("name=\"sender\"\r\n\r\n+15550000000\r\n").unwrap();
    let recipient = body.find("name=\"recipient\"\r\n\r\n+15551234567\r\n").unwrap();
    let message = body.find("name=\"message\"\r\n\r\ntitle\r\nbody\r\n").unwrap();
    let png = body.find("name=\"media[]\"; filename=\"pixel.png\"\r\nContent-Type: image/png\r\n\r\n").unwrap();
    let txt = body.find("name=\"media[]\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nnotes\r\n").unwrap();
    assert!(sender < recipient && recipient < message && message < png && png < txt);
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url("eight00com://token123@8005551234/15551234567").unwrap();
    for status in [201, 401, 422, 429, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap(), "status {}", status);
    }

    // One failed target fails the send, but the rest are still tried
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({"sender": "+8005551234", "recipient": "+15551234567", "message": "body"})))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("eight00com://token123@8005551234/15551234567/15551234568").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());

    // Nothing to notify
    let svc = from_url("eight00com://token123@9876543210/12").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());

    // Connection errors fail the send
    let svc = from_url("eight00com://token123@8005551234").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("body"))).await.unwrap());
  }
}
