use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{is_email, parse_list, valid_addresses};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd::{Qsd, unquote};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};
use std::time::Duration;

/// Pause between requests (upstream `request_rate_per_sec`).
const THROTTLE: Duration = Duration::from_millis(200);

/// SendGrid rejects an empty subject.
const DEFAULT_EMPTY_SUBJECT: &str = "<no subject>";

pub struct SendGrid {
  apikey: String,
  from_email: String,
  targets: Vec<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  /// Dynamic template ID (`?template=`).
  template: Option<String>,
  /// `+name=value` arguments, sent as `dynamic_template_data`.
  template_data: Vec<(String, String)>,
  throttle: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl SendGrid {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // sendgrid://{apikey}:{from_user}@{from_domain}/{targets}
    let qsd = Qsd::from_url(url);
    let apikey = url.user.clone().filter(|u| !u.is_empty())?;
    let password = url.password.clone().filter(|p| !p.is_empty())?;
    if !apikey.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
      tracing::warn!("An invalid SendGrid API Key was specified.");
      return None;
    }
    let from = format!("{}@{}", password, unquote(url.host.as_deref().unwrap_or_default()));
    let Some(from_email) = is_email(&from).map(|e| e.full_email) else {
      tracing::warn!("Invalid ~From~ email specified: {}", from);
      return None;
    };

    // Upstream runs every list through parse_list(), which sorts it
    let mut raw_targets = url.path_parts.clone();
    if let Some(to) = qsd.value("to") {
      raw_targets.extend(parse_list(&[to]));
    }
    let targets = if raw_targets.is_empty() {
      vec![from_email.clone()]
    } else {
      valid_addresses(&parse_list(&raw_targets), "To").into_iter().map(|e| e.full_email).collect()
    };
    let copies = |key: &str, kind: &str| -> Vec<String> {
      let mut list: Vec<String> = Vec::new();
      for e in valid_addresses(&parse_list(&[qsd.value(key).unwrap_or_default()]), kind) {
        if !list.contains(&e.full_email) {
          list.push(e.full_email);
        }
      }
      list
    };

    Some(Self {
      apikey,
      from_email,
      targets,
      cc: copies("cc", "Carbon Copy"),
      bcc: copies("bcc", "Blind Carbon Copy"),
      template: qsd.value("template").map(unquote),
      template_data: qsd.prefixed('+'),
      throttle: THROTTLE,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SendGrid",
      service_url: Some("https://sendgrid.com"),
      setup_url: Some("https://appriseit.com/services/sendgrid/"),
      protocols: vec!["sendgrid"],
      description: "Send email via SendGrid.",
      attachment_support: true,
    }
  }
}

#[async_trait]
impl Notify for SendGrid {
  fn schemas(&self) -> &[&str] {
    &["sendgrid"]
  }
  fn service_name(&self) -> &str {
    "SendGrid"
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
      tracing::warn!("There are no SendGrid email recipients to notify");
      return Ok(false);
    }

    let content_type = if ctx.body_format == NotifyFormat::Text { "text/plain" } else { "text/html" };
    let mut base = json!({
      "from": { "email": self.from_email },
      "subject": if ctx.title.is_empty() { DEFAULT_EMPTY_SUBJECT } else { ctx.title.as_str() },
      "content": [{ "type": content_type, "value": ctx.body }],
    });
    if !ctx.attachments.is_empty() {
      base["attachments"] = ctx
        .attachments
        .iter()
        .enumerate()
        .map(|(no, att)| {
          json!({
            "content": base64::engine::general_purpose::STANDARD.encode(&att.data),
            "filename": if att.name.is_empty() { format!("file{:03}.dat", no + 1) } else { att.name.clone() },
            "type": "application/octet-stream",
            "disposition": "attachment",
          })
        })
        .collect();
    }
    if let Some(template) = &self.template {
      base["template_id"] = json!(template);
    }

    let url = format!("{}/v3/mail/send", service_base("https://api.sendgrid.com"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for (index, target) in self.targets.iter().enumerate() {
      if index > 0 {
        tokio::time::sleep(self.throttle).await;
      }
      // To, Cc and Bcc must not repeat an address or SendGrid fails the request
      let emails = |list: Vec<&String>| Value::Array(list.into_iter().map(|e| json!({ "email": e })).collect());
      let mut personalization = Map::new();
      personalization.insert("to".into(), emails(vec![target]));
      if self.template.is_some() && !self.template_data.is_empty() {
        let data: Map<String, Value> = self.template_data.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        personalization.insert("dynamic_template_data".into(), Value::Object(data));
      }
      let cc: Vec<&String> = self.cc.iter().filter(|a| !self.bcc.contains(a) && *a != target).collect();
      if !cc.is_empty() {
        personalization.insert("cc".into(), emails(cc));
      }
      let bcc: Vec<&String> = self.bcc.iter().filter(|a| *a != target).collect();
      if !bcc.is_empty() {
        personalization.insert("bcc".into(), emails(bcc));
      }
      let mut payload = base.clone();
      payload["personalizations"] = json!([personalization]);

      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", self.apikey))
        .body(payload.to_string())
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 202) => tracing::info!("Sent SendGrid notification to {}.", target),
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send SendGrid notification to {}: {}error={}.", target, status_message(status), status);
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending SendGrid notification to {}.", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

/// Upstream `SENDGRID_HTTP_ERROR_MAP`, formatted for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    401 => "Unauthorized - You do not have authorization to make the request., ",
    413 => "Payload To Large - The JSON payload you have included in your request is too large., ",
    429 => "Too Many Requests - The number of requests you have made exceeds SendGrid's rate limitations., ",
    _ => "",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::method;
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> SendGrid {
    let mut sg = SendGrid::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url));
    sg.throttle = Duration::ZERO;
    sg
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
  fn test_valid_urls() {
    for url in [
      "sendgrid://abcd:user@example.com",
      "sendgrid://abcd:user@example.com/newuser@example.com",
      // Loads, but has no valid recipient
      "sendgrid://abcd:user@example.com/bademailaddress",
      "sendgrid://abcd:user@example.com/newuser@example.com?bcc=l2g@nuxref.com",
      "sendgrid://abcd:user@example.com/newuser@example.com?cc=l2g@nuxref.com",
      "sendgrid://abcd:user@example.com/newuser@example.com?to=l2g@nuxref.com",
      "sendgrid://abcd:user@example.com/newuser@example.com?template=8b799edf-6f98-4d3a-9be7-2862fb4e5752",
      "sendgrid://abcd:user@example.com/newuser@example.com?template=8b799edf-6f98-4d3a-9be7-2862fb4e5752&+sub=value&+sub2=value2",
      "sendgrid://abcd:user@example.ca/newuser@example.ca",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "sendgrid://",
      "sendgrid://:@/",
      "sendgrid://abcd",
      "sendgrid://abcd@host",
      "sendgrid://invalid-api-key+*-d:user@example.com",
      "sendgrid://abcd:!invalid@",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let sg = parse("sendgrid://abcd:user@example.com");
    assert_eq!((sg.apikey.as_str(), sg.from_email.as_str()), ("abcd", "user@example.com"));
    // No target: the sender is the recipient
    assert_eq!(sg.targets, vec!["user@example.com"]);

    // Recipients are sorted (upstream parse_list); invalid ones are dropped
    let sg =
      parse("sendgrid://abcd:user@example.com/zed@example.com/Named:named@example.com/bad?to=alpha@example.com&cc=cc@example.com,!invalid&bcc=bcc@example.com");
    assert_eq!(sg.targets, vec!["named@example.com", "alpha@example.com", "zed@example.com"]);
    assert_eq!(sg.cc, vec!["cc@example.com"]);
    assert_eq!(sg.bcc, vec!["bcc@example.com"]);

    let sg = parse("sendgrid://abcd:user@example.com/new@example.com?template=d-123&+sub=value&+sub2=v+2");
    assert_eq!(sg.template.as_deref(), Some("d-123"));
    assert_eq!(sg.template_data, vec![("sub".into(), "value".into()), ("sub2".into(), "v+2".into())]);
  }

  #[test]
  fn test_service_details() {
    let d = SendGrid::static_details();
    assert_eq!(d.service_name, "SendGrid");
    assert!(d.protocols.contains(&"sendgrid"));
    assert!(d.attachment_support);
  }

  #[tokio::test]
  async fn test_send_default_payload() {
    // Matches upstream (apprise-go golden "default")
    let server = mock(202).await;
    let svc = from_url("sendgrid://apikey:sender@domain.com/recipient@domain.com").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title", NotifyFormat::Html))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/v3/mail/send");
    assert_eq!(reqs[0].headers.get("authorization").unwrap(), "Bearer apikey");
    assert_eq!(reqs[0].headers.get("content-type").unwrap(), "application/json");
    assert_eq!(
      bodies(&server).await[0],
      json!({
        "personalizations": [{ "to": [{ "email": "recipient@domain.com" }] }],
        "from": { "email": "sender@domain.com" },
        "subject": "title",
        "content": [{ "type": "text/html", "value": "body" }],
      })
    );
  }

  #[tokio::test]
  async fn test_send_template_copies_and_text() {
    let server = mock(200).await;
    let svc = parse(
      "sendgrid://abcd:user@example.com/zed@example.com/alpha@example.com?cc=cc@example.com,zed@example.com&bcc=bcc@example.com,cc@example.com&template=d-123&+sub=value",
    );
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", NotifyFormat::Text))).await.unwrap());

    let bodies = bodies(&server).await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(
      bodies[0],
      json!({
        "personalizations": [{
          "to": [{ "email": "alpha@example.com" }],
          "dynamic_template_data": { "sub": "value" },
          "cc": [{ "email": "zed@example.com" }],
          "bcc": [{ "email": "bcc@example.com" }, { "email": "cc@example.com" }],
        }],
        "from": { "email": "user@example.com" },
        "subject": "<no subject>",
        "content": [{ "type": "text/plain", "value": "body" }],
        "template_id": "d-123",
      })
    );
    // zed is the recipient now, so it is not copied as well
    assert_eq!(bodies[1]["personalizations"][0]["to"], json!([{ "email": "zed@example.com" }]));
    assert!(bodies[1]["personalizations"][0].get("cc").is_none());
  }

  #[tokio::test]
  async fn test_send_attachments() {
    let server = mock(200).await;
    let svc = from_url("sendgrid://key:sender@example.com/user@example.com").unwrap();
    let mut ctx = ctx("title", NotifyFormat::Html);
    ctx.attachments = vec![
      Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() },
      Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "text/plain".into() },
    ];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(
      bodies(&server).await[0]["attachments"],
      json!([
        { "content": "UERGREFUQQ==", "filename": "report.pdf", "type": "application/octet-stream", "disposition": "attachment" },
        { "content": "eA==", "filename": "file002.dat", "type": "application/octet-stream", "disposition": "attachment" },
      ])
    );
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 429] {
      let server = mock(status).await;
      let svc = from_url("sendgrid://abcd:user@example.ca/newuser@example.ca").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap(), "status {}", status);
    }
    let server = mock(200).await;
    let svc = from_url("sendgrid://abcd:user@example.com/bademailaddress").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());
  }
}
