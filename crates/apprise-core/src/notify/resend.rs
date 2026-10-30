use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{formataddr, is_email, parse_emails_all, parse_list, valid_addresses};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd::{Qsd, unquote};
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

/// Pause between requests (upstream `request_rate_per_sec`).
const THROTTLE: Duration = Duration::from_millis(200);

/// Resend rejects an empty subject.
const DEFAULT_EMPTY_SUBJECT: &str = "<no subject>";

pub struct Resend {
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

impl Resend {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // resend://{apikey}:{from_user}@{from_domain}/{targets}
    // resend://{apikey}@{target}?from={from}
    let qsd = Qsd::from_url(url);
    let apikey = qsd.value("apikey").map(unquote).unwrap_or_else(|| url.user.clone().unwrap_or_default());
    if apikey.is_empty() || !apikey.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
      tracing::warn!("An invalid Resend API Key was specified.");
      return None;
    }

    let host = unquote(url.host.as_deref().unwrap_or_default());
    let mut raw_targets = Vec::new();
    let mut from = match qsd.value("from") {
      Some(from) => {
        // The host is a recipient when ?from= names the sender
        if !host.is_empty() {
          raw_targets.push(host);
        }
        unquote(from)
      }
      None => format!("{}@{}", url.password.clone().filter(|p| !p.is_empty()).or_else(|| url.user.clone()).unwrap_or_default(), host),
    };
    if let Some(name) = qsd.value("name") {
      from = formataddr(Some(&unquote(name)), &from);
    }
    let Some(sender) = is_email(&from) else {
      tracing::warn!("Invalid ~From~ email specified: {}", from);
      return None;
    };

    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = qsd.value("to") {
      raw_targets.extend(parse_list(&[to]));
    }
    let raw_targets = parse_emails_all(&raw_targets);
    let targets =
      if raw_targets.is_empty() { vec![sender.full_email.clone()] } else { valid_addresses(&raw_targets, "To").into_iter().map(|e| e.full_email).collect() };

    let mut names = HashMap::new();
    names.insert(sender.full_email.clone(), sender.display_name());
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
    let cc = collect(qsd.value("cc").map(|v| parse_list(&[v])).unwrap_or_default(), "Carbon Copy", true);
    let bcc = collect(qsd.value("bcc").map(|v| parse_list(&[v])).unwrap_or_default(), "Blind Carbon Copy", false);
    let reply_to = collect(qsd.value("reply").map(|v| vec![v.to_string()]).unwrap_or_default(), "Reply To", true);

    Some(Self {
      apikey,
      from_name: sender.display_name(),
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
      service_name: "Resend",
      service_url: Some("https://resend.com"),
      setup_url: Some("https://appriseit.com/services/resend/"),
      protocols: vec!["resend"],
      description: "Send email via Resend.",
      attachment_support: true,
    }
  }

  fn named(&self, addr: &str) -> String {
    formataddr(self.names.get(addr).and_then(|n| n.as_deref()), addr)
  }
}

#[async_trait]
impl Notify for Resend {
  fn schemas(&self) -> &[&str] {
    &["resend"]
  }
  fn service_name(&self) -> &str {
    "Resend"
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
    // Upstream sends nothing and reports success here; fail like Postmark
    if self.targets.is_empty() {
      tracing::warn!("There are no Resend email recipients to notify");
      return Ok(false);
    }

    let mut base = json!({
      "from": formataddr(self.from_name.as_deref(), &self.from_email),
      "subject": if ctx.title.is_empty() { DEFAULT_EMPTY_SUBJECT } else { ctx.title.as_str() },
    });
    base[if ctx.body_format == NotifyFormat::Text { "text" } else { "html" }] = json!(ctx.body);
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

    let url = format!("{}/emails", service_base("https://api.resend.com"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for (index, target) in self.targets.iter().enumerate() {
      if index > 0 {
        tokio::time::sleep(self.throttle).await;
      }
      let mut payload = base.clone();
      payload["to"] = json!(target);
      let cc: Vec<String> = self.cc.iter().filter(|a| !self.bcc.contains(a) && *a != target).map(|a| self.named(a)).collect();
      if !cc.is_empty() {
        payload["cc"] = json!(cc);
      }
      let bcc: Vec<&String> = self.bcc.iter().filter(|a| *a != target).collect();
      if !bcc.is_empty() {
        payload["bcc"] = json!(bcc);
      }
      let reply_to: Vec<String> = self.reply_to.iter().filter(|a| *a != target).map(|a| self.named(a)).collect();
      if !reply_to.is_empty() {
        payload["reply_to"] = json!(reply_to);
      }

      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", self.apikey))
        .body(payload.to_string())
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 202) => tracing::info!("Sent Resend notification to {}.", target),
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send Resend notification to {}: {}error={}.", target, status_message(status), status);
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Resend notification to {}.", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

/// Upstream `RESEND_HTTP_ERROR_MAP`, formatted for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    400 => "Check that the parameters were correct., ",
    401 => "The API key used was missing., ",
    403 => "The API key used was invalid., ",
    404 => "The resource was not found., ",
    429 => "The rate limit was exceeded., ",
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

  fn parse(url: &str) -> Resend {
    let mut r = Resend::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url));
    r.throttle = Duration::ZERO;
    r
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
      "resend://abcd:user@example.com",
      "resend://abcd:user@example.com/newuser1@example.com",
      "resend://abcd:user@example.com/newuser2@example.com?name=Jessica",
      "resend://abcd@newuser4%40example.com?name=Ralph&from=user2@example.ca",
      "resend://?apikey=abcd&from=Joe<user@example.com>&to=newuser5@example.com",
      "resend://?apikey=abcd&from=Joe<user@example.com>&reply=John<newuser6@example.com>",
      "resend://?apikey=abcd&from=Joe<user@example.com>&reply=garbage%",
      "resend://abcd:user@example.com/newuser7@example.com?bcc=l2g@nuxref.com",
      "resend://abcd:user@example.com/newuser8@example.com?cc=l2g@nuxref.com",
      "resend://abcd:user@example.com/newuser8@example.com?cc=Chris<l2g@nuxref.com>",
      "resend://abcd:user@example.com/newuser9@example.com?to=l2g@nuxref.com",
      "resend://abcd:user@example.ca/newuser0@example.ca",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["resend://", "resend://:@/", "resend://abcd", "resend://abcd@host", "resend://invalid-api-key+*-d:user@example.com"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let r = parse("resend://abcd:user@example.com");
    assert_eq!((r.apikey.as_str(), r.from_email.as_str(), r.from_name.clone()), ("abcd", "user@example.com", None));
    assert_eq!(r.targets, vec!["user@example.com"]);

    // ?from= turns the host into a recipient
    let r = parse("resend://abcd@newuser4%40example.com?name=Ralph&from=user2@example.ca");
    assert_eq!((r.from_name.as_deref(), r.from_email.as_str()), (Some("Ralph"), "user2@example.ca"));
    assert_eq!(r.targets, vec!["newuser4@example.com"]);

    let r = parse("resend://?apikey=abcd&from=Joe<user@example.com>&reply=John<newuser6@example.com>,garbage&cc=Chris<l2g@nuxref.com>&bcc=b@example.com");
    assert_eq!((r.from_name.as_deref(), r.from_email.as_str()), (Some("Joe"), "user@example.com"));
    assert_eq!(r.reply_to, vec!["newuser6@example.com"]);
    assert_eq!(r.cc, vec!["l2g@nuxref.com"]);
    assert_eq!(r.bcc, vec!["b@example.com"]);
    assert_eq!(r.names.get("l2g@nuxref.com"), Some(&Some("Chris".to_string())));

    // name= keeps a literal '+'
    assert_eq!(parse("resend://abcd:user@example.com?name=Dr.+Who").from_name.as_deref(), Some("Dr.+Who"));
  }

  #[test]
  fn test_service_details() {
    let d = Resend::static_details();
    assert_eq!(d.service_name, "Resend");
    assert!(d.protocols.contains(&"resend"));
    assert!(d.attachment_support);
  }

  #[tokio::test]
  async fn test_send_default_payload() {
    // Matches upstream (apprise-go golden "default")
    let server = mock(200).await;
    let svc = from_url("resend://apikey:sender@domain.com/recipient@domain.com").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title", NotifyFormat::Html))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/emails");
    assert_eq!(reqs[0].headers.get("authorization").unwrap(), "Bearer apikey");
    assert_eq!(reqs[0].headers.get("content-type").unwrap(), "application/json");
    assert_eq!(bodies(&server).await[0], json!({ "from": "sender@domain.com", "subject": "title", "html": "body", "to": "recipient@domain.com" }));
  }

  #[tokio::test]
  async fn test_send_copies_reply_and_names() {
    // Verified against upstream with pycapture; the unnamed reply-to entry
    // for the sender clears its name, as upstream does
    let server = mock(202).await;
    let svc = parse(
      "resend://?apikey=abcd&from=Joe<user@example.com>&reply=John<newuser6@example.com>,user@example.com&cc=Chris<l2g@nuxref.com>,new@example.com&bcc=b@example.com,l2g@nuxref.com&to=new@example.com,zz@example.com",
    );
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", NotifyFormat::Html))).await.unwrap());
    assert_eq!(
      bodies(&server).await,
      vec![
        json!({
          "from": "Joe <user@example.com>",
          "subject": "<no subject>",
          "html": "body",
          "to": "new@example.com",
          "bcc": ["b@example.com", "l2g@nuxref.com"],
          "reply_to": ["John <newuser6@example.com>", "user@example.com"],
        }),
        json!({
          "from": "Joe <user@example.com>",
          "subject": "<no subject>",
          "html": "body",
          "to": "zz@example.com",
          "cc": ["new@example.com"],
          "bcc": ["b@example.com", "l2g@nuxref.com"],
          "reply_to": ["John <newuser6@example.com>", "user@example.com"],
        }),
      ]
    );
  }

  #[tokio::test]
  async fn test_send_text_name_and_attachments() {
    let server = mock(200).await;
    let svc = parse("resend://abcd:user@example.com?name=Dr.+Who");
    let mut ctx = ctx("title", NotifyFormat::Text);
    ctx.attachments = vec![Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() }];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(
      bodies(&server).await[0],
      json!({
        "from": "\"Dr.+Who\" <user@example.com>",
        "subject": "title",
        "text": "body",
        "attachments": [{ "content": "UERGREFUQQ==", "filename": "report.pdf", "type": "application/octet-stream", "disposition": "attachment" }],
        "to": "user@example.com",
      })
    );
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 403] {
      let server = mock(status).await;
      let svc = from_url("resend://abcd:user@example.ca/newuser0@example.ca").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap(), "status {}", status);
    }
    let server = mock(200).await;
    let svc = from_url("resend://abcd:user@example.com/bad").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());
  }
}
