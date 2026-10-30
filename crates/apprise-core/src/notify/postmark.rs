use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{formataddr, is_email, parse_emails_all, parse_list, valid_addresses};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd::{Qsd, unquote, validate_token};
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

/// Pause between requests (upstream `request_rate_per_sec`).
const THROTTLE: Duration = Duration::from_millis(1200);

/// Subject used when the notification has no title.
const DEFAULT_EMPTY_SUBJECT: &str = "<no subject>";

pub struct Postmark {
  /// Server API token.
  apikey: String,
  from_name: Option<String>,
  from_email: String,
  targets: Vec<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  reply_to: Vec<String>,
  names: HashMap<String, Option<String>>,
  throttle: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Postmark {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // postmark://{apikey}:{from_user}@{from_domain}/{targets}
    // postmark://{apikey}@{target}?from={from}
    let qsd = Qsd::from_url(url);
    let apikey = qsd.value("apikey").map(unquote).unwrap_or_else(|| url.user.clone().unwrap_or_default());
    let Some(apikey) = validate_token(&apikey) else {
      tracing::warn!("An invalid Postmark API Key was specified.");
      return None;
    };

    let host = unquote(url.host.as_deref().unwrap_or_default());
    let mut raw_targets = Vec::new();
    let from = match qsd.value("from") {
      Some(from) => {
        // The host is a recipient when ?from= names the sender
        if !host.is_empty() {
          raw_targets.push(host);
        }
        unquote(from)
      }
      None => format!("{}@{}", url.password.clone().filter(|p| !p.is_empty()).or_else(|| url.user.clone()).unwrap_or_default(), host),
    };
    let Some(sender) = is_email(&from) else {
      tracing::warn!("An invalid Postmark From email ({}) was specified.", from);
      return None;
    };
    // ?name= overrides a name given with the address
    let from_name = qsd.value("name").map(unquote).or_else(|| sender.display_name());

    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = qsd.value("to") {
      raw_targets.extend(parse_list(&[to]));
    }

    let mut names = HashMap::new();
    let mut collect = |raw: Vec<String>, kind: &str, named: bool| {
      let mut list: Vec<String> = Vec::new();
      for e in valid_addresses(&parse_emails_all(&raw), kind) {
        if named {
          names.insert(e.full_email.clone(), e.display_name());
        }
        if !list.contains(&e.full_email) {
          list.push(e.full_email);
        }
      }
      list
    };
    let targets = if raw_targets.is_empty() { vec![sender.full_email.clone()] } else { collect(raw_targets, "Postmark To", true) };
    let cc = collect(qsd.value("cc").map(|v| parse_list(&[v])).unwrap_or_default(), "Postmark Carbon Copy", true);
    let bcc = collect(qsd.value("bcc").map(|v| parse_list(&[v])).unwrap_or_default(), "Postmark Blind Carbon Copy", false);
    let reply_to = collect(qsd.value("reply").map(|v| vec![v.to_string()]).unwrap_or_default(), "Postmark Reply To", true);

    Some(Self {
      apikey,
      from_name,
      from_email: sender.full_email,
      targets,
      cc,
      bcc,
      reply_to,
      names,
      throttle: THROTTLE,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Postmark",
      service_url: Some("https://postmarkapp.com/"),
      setup_url: Some("https://appriseit.com/services/postmark/"),
      protocols: vec!["postmark"],
      description: "Send email via Postmark.",
      attachment_support: true,
    }
  }

  fn named(&self, addr: &str) -> String {
    formataddr(self.names.get(addr).and_then(|n| n.as_deref()), addr)
  }
}

#[async_trait]
impl Notify for Postmark {
  fn schemas(&self) -> &[&str] {
    &["postmark"]
  }
  fn service_name(&self) -> &str {
    "Postmark"
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
    1.2
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Postmark email recipients to notify");
      return Ok(false);
    }

    let mut base = json!({
      "From": formataddr(self.from_name.as_deref(), &self.from_email),
      "Subject": if ctx.title.is_empty() { DEFAULT_EMPTY_SUBJECT } else { ctx.title.as_str() },
    });
    base[if ctx.body_format == NotifyFormat::Html { "HtmlBody" } else { "TextBody" }] = json!(ctx.body);
    if !ctx.attachments.is_empty() {
      base["Attachments"] = ctx
        .attachments
        .iter()
        .enumerate()
        .map(|(no, att)| {
          json!({
            "Name": if att.name.is_empty() { format!("file{:03}.dat", no + 1) } else { att.name.clone() },
            "Content": base64::engine::general_purpose::STANDARD.encode(&att.data),
            "ContentType": att.mime_type,
          })
        })
        .collect();
    }

    let url = format!("{}/email", service_base("https://api.postmarkapp.com"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for (index, target) in self.targets.iter().enumerate() {
      if index > 0 {
        tokio::time::sleep(self.throttle).await;
      }
      let mut payload = base.clone();
      payload["To"] = json!(target);
      let cc: Vec<String> = self.cc.iter().filter(|a| !self.bcc.contains(a) && *a != target).map(|a| self.named(a)).collect();
      if !cc.is_empty() {
        payload["Cc"] = json!(cc.join(","));
      }
      let bcc: Vec<&str> = self.bcc.iter().filter(|a| *a != target).map(|a| a.as_str()).collect();
      if !bcc.is_empty() {
        payload["Bcc"] = json!(bcc.join(","));
      }
      let reply_to: Vec<String> = self.reply_to.iter().filter(|a| *a != target).map(|a| self.named(a)).collect();
      if !reply_to.is_empty() {
        payload["ReplyTo"] = json!(reply_to.join(","));
      }

      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("X-Postmark-Server-Token", &self.apikey)
        .body(payload.to_string())
        .send()
        .await;
      match result {
        Ok(resp) if resp.status().as_u16() == 200 => tracing::info!("Sent Postmark notification to {}.", target),
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send Postmark notification to {}: {}error={}.", target, status_message(status), status);
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Postmark notification to {}.", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

/// Upstream `POSTMARK_HTTP_ERROR_MAP`, formatted for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    401 => "Unauthorized - Invalid or missing Server API Token., ",
    422 => "Unprocessable Entity - Invalid payload or configuration., ",
    429 => "Too Many Requests - Rate limit exceeded., ",
    500 => "Internal Server Error., ",
    _ => "",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use serde_json::Value;
  use wiremock::matchers::method;
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Postmark {
    let mut p = Postmark::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url));
    p.throttle = Duration::ZERO;
    p
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  async fn bodies(server: &MockServer) -> Vec<Value> {
    server.received_requests().await.unwrap().iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  fn ctx(title: &str, body_format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: "body".into(), body_format, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "postmark://abcd:user@example.com",
      "postmark://abcd:user@example.com?format=text",
      "postmark://abcd:user@example.com/newuser1@example.com",
      "postmark://abcd:user@example.com/newuser2@example.com?name=John",
      "postmark://abcd:user@example.com/newuser3@example.com?reply=reply@example.com",
      "postmark://abcd:user@example.com/newuser4@example.com?bcc=bcc@nuxref.com",
      "postmark://abcd:user@example.com/newuser5@example.com?cc=cc@nuxref.com",
      "postmark://abcd:user@example.com/newuser5@example.com?cc=Chris<cc@nuxref.com>",
      "postmark://abcd:user@example.com/newuser6@example.com?to=extra@nuxref.com",
      // Loads, but has no valid recipient
      "postmark://abcd:user@example.com/bademailaddress",
      "postmark://abcd:user@example.ca/newuser@example.ca",
      "postmark://apikey123@example.com/target@example.com?from=sender@example.com&name=Apprise",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["postmark://", "postmark://:@/", "postmark://abcd", "postmark://abcd@host"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let p = parse("postmark://abcd:user@example.com");
    assert_eq!((p.apikey.as_str(), p.from_email.as_str(), p.from_name.clone()), ("abcd", "user@example.com", None));
    assert_eq!(p.targets, vec!["user@example.com"]);

    // ?apikey= and ?from= override the URL parts; the host becomes a target
    let p = parse("postmark://abcd@target%40example.com?apikey=overridekey&from=Sender<sender@example.com>&name=Alice");
    assert_eq!(p.apikey, "overridekey");
    assert_eq!((p.from_name.as_deref(), p.from_email.as_str()), (Some("Alice"), "sender@example.com"));
    assert_eq!(p.targets, vec!["target@example.com"]);

    let p = parse("postmark://abcd:user@example.com/new@example.com/bad?cc=Chris<cc@example.com>,!bad&bcc=bcc@example.com&reply=Rep<r@example.com>");
    assert_eq!(p.targets, vec!["new@example.com"]);
    assert_eq!(p.cc, vec!["cc@example.com"]);
    assert_eq!(p.bcc, vec!["bcc@example.com"]);
    assert_eq!(p.reply_to, vec!["r@example.com"]);
    assert_eq!(p.names.get("r@example.com"), Some(&Some("Rep".to_string())));
  }

  #[test]
  fn test_service_details() {
    let d = Postmark::static_details();
    assert_eq!(d.service_name, "Postmark");
    assert!(d.protocols.contains(&"postmark"));
    assert!(d.attachment_support);
  }

  #[tokio::test]
  async fn test_send_default_payload() {
    // Matches upstream (apprise-go golden "default")
    let server = mock(200).await;
    let svc = from_url("postmark://apikey123:sender@example.com/target@example.com").unwrap();
    assert_eq!(svc.notify_format(), NotifyFormat::Html);
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("apprise parity", NotifyFormat::Html))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/email");
    assert_eq!(reqs[0].headers.get("x-postmark-server-token").unwrap(), "apikey123");
    assert_eq!(reqs[0].headers.get("accept").unwrap(), "application/json");
    assert_eq!(reqs[0].headers.get("content-type").unwrap(), "application/json");
    assert_eq!(bodies(&server).await[0], json!({ "From": "sender@example.com", "Subject": "apprise parity", "HtmlBody": "body", "To": "target@example.com" }));
  }

  #[tokio::test]
  async fn test_send_from_query() {
    // Matches upstream (apprise-go golden "from-query")
    let server = mock(200).await;
    let svc = from_url("postmark://apikey123@example.com/target@example.com?from=sender@example.com&name=Apprise").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", NotifyFormat::Html))).await.unwrap());
    assert_eq!(
      bodies(&server).await,
      vec![json!({ "From": "Apprise <sender@example.com>", "Subject": "<no subject>", "HtmlBody": "body", "To": "target@example.com" })]
    );
  }

  #[tokio::test]
  async fn test_send_copies_and_reply_to() {
    // Verified against upstream with pycapture
    let server = mock(200).await;
    let svc = parse(
      "postmark://abcd:user@example.com/Tom:t@example.com/b@example.com?cc=Chris<cc@example.com>,cc2@example.com&bcc=bcc@example.com,cc2@example.com&reply=Rep<r@example.com>,t@example.com&name=Sender",
    );
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", NotifyFormat::Text))).await.unwrap());
    assert_eq!(
      bodies(&server).await,
      vec![
        json!({
          "From": "Sender <user@example.com>",
          "Subject": "<no subject>",
          "TextBody": "body",
          "To": "t@example.com",
          "Cc": "Chris <cc@example.com>",
          "Bcc": "bcc@example.com,cc2@example.com",
          "ReplyTo": "Rep <r@example.com>",
        }),
        json!({
          "From": "Sender <user@example.com>",
          "Subject": "<no subject>",
          "TextBody": "body",
          "To": "b@example.com",
          "Cc": "Chris <cc@example.com>",
          "Bcc": "bcc@example.com,cc2@example.com",
          "ReplyTo": "Rep <r@example.com>,t@example.com",
        }),
      ]
    );
  }

  #[tokio::test]
  async fn test_send_attachments_and_encoded_name() {
    let server = mock(200).await;
    let svc = parse("postmark://token:sender@example.com/user@example.com?name=Jörg");
    let mut ctx = ctx("title", NotifyFormat::Html);
    ctx.attachments = vec![
      Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() },
      Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "text/plain".into() },
    ];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let body = &bodies(&server).await[0];
    assert_eq!(body["From"], json!("=?utf-8?q?J=C3=B6rg?= <sender@example.com>"));
    assert_eq!(
      body["Attachments"],
      json!([
        { "Name": "report.pdf", "Content": "UERGREFUQQ==", "ContentType": "application/pdf" },
        { "Name": "file002.dat", "Content": "eA==", "ContentType": "text/plain" },
      ])
    );
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 422, 202] {
      let server = mock(status).await;
      let svc = from_url("postmark://abcd:user@example.ca/newuser@example.ca").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap(), "status {}", status);
    }
    let server = mock(200).await;
    let svc = from_url("postmark://abcd:user@example.com/bademailaddress").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());
  }
}
