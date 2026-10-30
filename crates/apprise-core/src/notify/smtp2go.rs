use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{formataddr, is_email, parse_emails, parse_emails_all, valid_addresses};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::qsd::{Qsd, unquote, validate_token};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// Recipients per request in batch mode.
const BATCH_SIZE: usize = 100;

/// Pause between requests (upstream `request_rate_per_sec`).
const THROTTLE: Duration = Duration::from_millis(200);

pub struct Smtp2Go {
  apikey: String,
  from_name: Option<String>,
  from_email: String,
  /// `(display name, address)`
  targets: Vec<(Option<String>, String)>,
  cc: Vec<String>,
  bcc: Vec<String>,
  names: HashMap<String, Option<String>>,
  /// `+name=value` arguments, sent as `custom_headers`.
  headers: Vec<(String, String)>,
  batch: bool,
  throttle: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Smtp2Go {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let qsd = Qsd::from_url(url);

    // smtp2go://{user}@{domain}/{apikey}/{targets}
    let mut path = url.path_parts.iter();
    let apikey = path.next().and_then(|k| validate_token(k))?;
    let user = url.user.clone().filter(|u| !u.is_empty())?;

    let from_name = qsd.value("name").map(unquote);
    let from_email = format!("{}@{}", user, url.host.clone().unwrap_or_default());
    if is_email(&from_email).is_none() {
      tracing::warn!("Invalid ~From~ email format: {}", from_email);
      return None;
    }

    let mut raw_targets: Vec<String> = path.cloned().collect();
    if let Some(to) = qsd.value("to") {
      raw_targets.push(to.to_string());
    }
    let targets = if raw_targets.is_empty() {
      vec![(from_name.clone().filter(|n| !n.is_empty()), from_email.clone())]
    } else {
      valid_addresses(&parse_emails_all(&raw_targets), "To").into_iter().map(|e| (e.display_name(), e.full_email)).collect()
    };

    let mut names = HashMap::new();
    let mut cc = Vec::new();
    let mut bcc = Vec::new();
    for (list, raw, kind) in [(&mut cc, qsd.value("cc"), "Carbon Copy"), (&mut bcc, qsd.value("bcc"), "Blind Carbon Copy")] {
      for e in valid_addresses(&parse_emails(raw.unwrap_or_default()), kind) {
        names.insert(e.full_email.clone(), e.display_name());
        if !list.contains(&e.full_email) {
          list.push(e.full_email);
        }
      }
    }

    Some(Self {
      apikey,
      from_name,
      from_email,
      targets,
      cc,
      bcc,
      names,
      headers: qsd.prefixed('+').into_iter().map(|(k, v)| (unquote(&k), unquote(&v))).collect(),
      batch: qsd.get("batch").map(parse_bool).unwrap_or(false),
      throttle: THROTTLE,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SMTP2Go",
      service_url: Some("https://www.smtp2go.com/"),
      setup_url: Some("https://appriseit.com/services/smtp2go/"),
      protocols: vec!["smtp2go"],
      description: "Send email via SMTP2Go.",
      attachment_support: true,
    }
  }
}

#[async_trait]
impl Notify for Smtp2Go {
  fn schemas(&self) -> &[&str] {
    &["smtp2go"]
  }
  fn service_name(&self) -> &str {
    "SMTP2Go"
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
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Html
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.2
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Email recipients to notify");
      return Ok(false);
    }

    let url = format!("{}/v3/email/send", service_base("https://api.smtp2go.com"));
    let client = build_client(self.verify_certificate)?;
    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for (index, batch) in self.targets.chunks(batch_size).enumerate() {
      if index > 0 {
        tokio::time::sleep(self.throttle).await;
      }
      let in_batch = |addr: &String| batch.iter().any(|(_, t)| t == addr);
      let mut payload = json!({
        "api_key": self.apikey,
        "sender": formataddr(self.from_name.as_deref(), &self.from_email),
        "subject": ctx.title,
        "to": batch.iter().map(|(name, addr)| formataddr(name.as_deref(), addr)).collect::<Vec<_>>(),
      });
      if !ctx.attachments.is_empty() {
        payload["attachments"] = ctx
          .attachments
          .iter()
          .enumerate()
          .map(|(no, att)| {
            json!({
              "filename": if att.name.is_empty() { format!("file{:03}.dat", no + 1) } else { att.name.clone() },
              "fileblob": base64::engine::general_purpose::STANDARD.encode(&att.data),
              "mimetype": att.mime_type,
            })
          })
          .collect();
      }
      payload[if ctx.body_format == NotifyFormat::Html { "html_body" } else { "text_body" }] = json!(ctx.body);
      let cc: Vec<String> =
        self.cc.iter().filter(|a| !self.bcc.contains(a) && !in_batch(a)).map(|a| formataddr(self.names.get(a).and_then(|n| n.as_deref()), a)).collect();
      if !cc.is_empty() {
        payload["cc"] = json!(cc);
      }
      let bcc: Vec<&String> = self.bcc.iter().filter(|a| !in_batch(a)).collect();
      if !bcc.is_empty() {
        payload["bcc"] = json!(bcc);
      }
      if !self.headers.is_empty() {
        payload["custom_headers"] = Value::Array(self.headers.iter().map(|(k, v)| json!({ "header": k, "value": v })).collect());
      }

      let dest = if batch.len() <= 3 { batch.iter().map(|(_, a)| a.as_str()).collect::<Vec<_>>().join(", ") } else { format!("{} recipients", batch.len()) };
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .send()
        .await;
      match result {
        Ok(resp) if resp.status().as_u16() == 200 => tracing::info!("Sent SMTP2Go notification to {}.", dest),
        Ok(resp) => {
          let status = resp.status().as_u16();
          let detail = if status == 429 { "To many requests., " } else { "" };
          tracing::warn!("Failed to send SMTP2Go notification to {}: {}error={}.", dest, detail, status);
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending SMTP2Go:{} notification.", dest);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::method;
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn key() -> String {
    format!("{}-{}-{}", "a".repeat(32), "b".repeat(8), "c".repeat(8))
  }

  fn parse(url: &str) -> Smtp2Go {
    let mut s = Smtp2Go::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url));
    s.throttle = Duration::ZERO;
    s
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  async fn bodies(server: &MockServer) -> Vec<Value> {
    server
      .received_requests()
      .await
      .unwrap()
      .iter()
      .filter(|r| r.method == wiremock::http::Method::POST)
      .map(|r| serde_json::from_slice(&r.body).unwrap())
      .collect()
  }

  fn ctx(title: &str, body_format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: "body".into(), body_format, ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let k = key();
    for url in [
      "smtp2go://".to_string(),
      "smtp2go://:@/".to_string(),
      "smtp2go://user@localhost.localdomain".to_string(),
      format!("smtp2go://localhost.localdomain/{}", k),
      format!("smtp2go://\"@localhost.localdomain/{}", k),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let k = key();
    for url in [
      format!("smtp2go://user@localhost.localdomain/{}", k),
      format!("smtp2go://user@localhost.localdomain/{}?format=markdown", k),
      format!("smtp2go://user@localhost.localdomain/{}?format=html", k),
      format!("smtp2go://user@localhost.localdomain/{}?format=text", k),
      format!("smtp2go://user@localhost.localdomain/{}?+X-Customer-Campaign-ID=Apprise", k),
      format!("smtp2go://user@localhost.localdomain/{}?bcc=user@example.com&cc=user2@example.com", k),
      format!("smtp2go://user@localhost.localdomain/{}/test@example.com", k),
      format!("smtp2go://user@localhost.localdomain/{}?to=test@example.com", k),
      format!("smtp2go://user@localhost.localdomain/{}/test@example.com?name=\"Frodo\"", k),
      // Loads, but has no valid recipient
      format!("smtp2go://user@localhost.localdomain/{}/invalid", k),
      format!(
        "smtp2go://user@example.com/{}/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com",
        k
      ),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let s = parse(&format!(
      "smtp2go://user@example.com/{}/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com&+X-One=1",
      key()
    ));
    assert_eq!(s.apikey, key());
    assert_eq!(s.targets, vec![(None, "user1@example.com".to_string()), (Some("User2".to_string()), "user2@example.com".to_string())]);
    assert_eq!(s.cc, vec!["user4@example.com", "user5@example.com"]);
    assert_eq!(s.bcc, vec!["user3@example.com", "user1@example.com"]);
    assert_eq!(s.headers, vec![("X-One".to_string(), "1".to_string())]);
    assert!(!s.batch);
  }

  #[test]
  fn test_static_details() {
    let details = Smtp2Go::static_details();
    assert_eq!(details.service_name, "SMTP2Go");
    assert!(details.protocols.contains(&"smtp2go"));
    assert!(details.attachment_support);
  }

  #[tokio::test]
  async fn test_send_default_payload() {
    // Matches upstream (apprise-go golden "default")
    let server = mock(200).await;
    let svc = from_url("smtp2go://sender@domain.com/apikey/recipient@domain.com").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title", NotifyFormat::Html))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/v3/email/send");
    assert_eq!(reqs[0].headers.get("accept").unwrap(), "application/json");
    assert_eq!(reqs[0].headers.get("content-type").unwrap(), "application/json");
    assert_eq!(
      bodies(&server).await[0],
      json!({ "api_key": "apikey", "sender": "sender@domain.com", "subject": "title", "to": ["recipient@domain.com"], "html_body": "body" })
    );
  }

  #[tokio::test]
  async fn test_send_batch_names_copies_headers() {
    // Verified against upstream with pycapture
    let server = mock(200).await;
    let svc = parse(
      "smtp2go://user@example.com/apikey/user1@example.com/Jo:jo@example.com?cc=Chris<cc@example.com>,cc2@example.com,jo@example.com&bcc=bcc@example.com&+X-Header=val&+X-Two=2&name=Sender&batch=yes",
    );
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title", NotifyFormat::Text))).await.unwrap());
    assert_eq!(
      bodies(&server).await,
      vec![json!({
        "api_key": "apikey",
        "sender": "Sender <user@example.com>",
        "subject": "title",
        "to": ["user1@example.com", "Jo <jo@example.com>"],
        "text_body": "body",
        "cc": ["Chris <cc@example.com>", "cc2@example.com"],
        "bcc": ["bcc@example.com"],
        "custom_headers": [{ "header": "X-Header", "value": "val" }, { "header": "X-Two", "value": "2" }],
      })]
    );
  }

  #[tokio::test]
  async fn test_send_user_is_sender_and_attachments() {
    // Matches upstream (apprise-go golden "attachment"): the user is the
    // sender and the first path entry the API key
    let server = mock(200).await;
    let svc = parse("smtp2go://key:sender@example.com/user@example.com/second@example.com");
    let mut ctx = ctx("title", NotifyFormat::Html);
    ctx.attachments = vec![Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() }];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let bodies = bodies(&server).await;
    assert_eq!(
      bodies[0],
      json!({
        "api_key": "user@example.com",
        "sender": "key@example.com",
        "subject": "title",
        "to": ["second@example.com"],
        "attachments": [{ "filename": "report.pdf", "fileblob": "UERGREFUQQ==", "mimetype": "application/pdf" }],
        "html_body": "body",
      })
    );
    assert_eq!(bodies.len(), 1);
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 429] {
      let server = mock(status).await;
      let svc = parse(&format!("smtp2go://user@localhost.localdomain/{}", key()));
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap(), "status {}", status);
    }
    let server = mock(200).await;
    let svc = parse(&format!("smtp2go://user@localhost.localdomain/{}/invalid", key()));
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());
  }
}
