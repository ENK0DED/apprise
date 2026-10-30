use crate::conversion::convert_between;
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::email::{is_email, parse_list};
use crate::utils::parse::ParsedUrl;
use crate::utils::sanitize::sanitize_payload;
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

/// MailerSend rejects an empty subject.
const DEFAULT_EMPTY_SUBJECT: &str = "<no subject>";

pub struct MailerSend {
  apikey: String,
  from_email: String,
  targets: Vec<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  reply_to: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream's API key regex `^[a-zA-Z0-9._-]+$`.
fn is_apikey(key: &str) -> bool {
  !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The valid addresses in `entries` (upstream `parse_list()` then
/// `is_email()`), each kept once.
fn unique_addresses(entries: &[String], kind: &str) -> Vec<String> {
  let mut out: Vec<String> = Vec::new();
  for entry in parse_list(entries) {
    match is_email(&entry) {
      Some(e) if !out.contains(&e.full_email) => out.push(e.full_email),
      Some(_) => {}
      None => tracing::warn!("Dropped invalid MailerSend {}email ({}) specified.", kind, entry),
    }
  }
  out
}

impl MailerSend {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // mailersend://{apikey}:{from_user}@{from_domain}/{targets}
    let host = url.verified_host()?;
    let apikey = url.user.as_deref()?;
    let from_user = url.password.as_deref()?;

    if !is_apikey(apikey) {
      tracing::warn!("An invalid MailerSend API Key was specified.");
      return None;
    }

    let from = format!("{}@{}", from_user, host);
    let Some(sender) = is_email(&from) else {
      tracing::warn!("Invalid MailerSend From email specified: {}", from);
      return None;
    };

    let reply_to = match url.get("reply").filter(|r| !r.is_empty()) {
      Some(reply) => match is_email(reply) {
        Some(e) => Some(e.full_email),
        None => {
          tracing::warn!("An invalid MailerSend Reply To ({}) was specified.", reply);
          return None;
        }
      },
      None => None,
    };

    let mut raw_targets = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_list(&[to]));
    }
    // Without any target the sender notifies itself
    let targets: Vec<String> = if raw_targets.is_empty() {
      vec![sender.full_email.clone()]
    } else {
      parse_list(&raw_targets)
        .into_iter()
        .filter_map(|recipient| {
          let parsed = is_email(&recipient);
          if parsed.is_none() {
            tracing::warn!("Dropped invalid MailerSend email ({}) specified.", recipient);
          }
          parsed.map(|e| e.full_email)
        })
        .collect()
    };

    let list = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(|v| parse_list(&[v])).unwrap_or_default();
    let cc = unique_addresses(&list("cc"), "Carbon Copy ");
    let bcc = unique_addresses(&list("bcc"), "Blind Carbon Copy ");

    Some(Self {
      apikey: apikey.to_string(),
      from_email: sender.full_email,
      targets,
      cc,
      bcc,
      reply_to,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "MailerSend",
      service_url: Some("https://www.mailersend.com/"),
      setup_url: Some("https://appriseit.com/services/mailersend/"),
      protocols: vec!["mailersend"],
      description: "Send email via MailerSend.",
      attachment_support: true,
    }
  }
}

/// Upstream `MAILERSEND_HTTP_ERROR_MAP` over the base HTTP lookup,
/// formatted for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Unsupported Parameters., ",
    401 => "Unauthorized - Invalid or missing API token., ",
    403 => "Forbidden - API token lacks send permission., ",
    404 => "Page not found., ",
    405 => "Method not allowed., ",
    422 => "Unprocessable Entity - Validation error in request., ",
    429 => "Too Many Requests - Rate limit exceeded., ",
    500 => "Internal server error., ",
    503 => "Servers are overloaded., ",
    _ => "",
  }
}

#[async_trait]
impl Notify for MailerSend {
  fn schemas(&self) -> &[&str] {
    &["mailersend"]
  }
  fn service_name(&self) -> &str {
    "MailerSend"
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
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Html, NotifyFormat::Text]
  }
  fn request_rate_per_sec(&self) -> f64 {
    // 300 requests per minute
    0.2
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["mailersend".into(), self.apikey.clone(), self.from_email.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no MailerSend email recipients to notify");
      return Ok(false);
    }

    let mut base = json!({
      "from": { "email": self.from_email },
      "subject": if ctx.title.is_empty() { DEFAULT_EMPTY_SUBJECT } else { ctx.title.as_str() },
    });
    // Both parts are sent; the other one is derived from the body
    if ctx.body_format == NotifyFormat::Text {
      base["text"] = json!(ctx.body);
      base["html"] = json!(convert_between(&NotifyFormat::Text, &NotifyFormat::Html, &ctx.body));
    } else {
      base["html"] = json!(ctx.body);
      base["text"] = json!(convert_between(&NotifyFormat::Html, &NotifyFormat::Text, &ctx.body));
    }
    if !ctx.attachments.is_empty() {
      base["attachments"] = ctx
        .attachments
        .iter()
        .enumerate()
        .map(|(i, att)| {
          json!({
            "content": base64::engine::general_purpose::STANDARD.encode(&att.data),
            "filename": if att.name.is_empty() { format!("file{:03}.dat", i + 1) } else { att.name.clone() },
            "disposition": "attachment",
          })
        })
        .collect();
    }
    if let Some(reply_to) = &self.reply_to {
      base["reply_to"] = json!({ "email": reply_to });
    }

    let url = format!("{}/v1/email", service_base("https://api.mailersend.com"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for target in &self.targets {
      // A retry skips the targets that already accepted the message
      if is_delivered(target, false) {
        continue;
      }

      // The cc and bcc lists must not hold the target
      let mut payload = base.clone();
      payload["to"] = json!([{ "email": target }]);
      let cc: Vec<Value> = self.cc.iter().filter(|a| !self.bcc.contains(a) && *a != target).map(|a| json!({ "email": a })).collect();
      if !cc.is_empty() {
        payload["cc"] = json!(cc);
      }
      let bcc: Vec<Value> = self.bcc.iter().filter(|a| *a != target).map(|a| json!({ "email": a })).collect();
      if !bcc.is_empty() {
        payload["bcc"] = json!(bcc);
      }

      if tracing::enabled!(tracing::Level::DEBUG) {
        tracing::debug!("MailerSend POST URL: {} (cert_verify={})", url, self.verify_certificate);
        tracing::debug!("MailerSend Payload: {}", sanitize_payload(&payload));
      }

      throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", self.apikey))
        .body(payload.to_string())
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 202) => {
          tracing::info!("Sent MailerSend notification to {}.", target);
          mark_delivered(target, false);
        }
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send MailerSend notification to {}: {}error={}.", target, status_message(status), status);
          tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending MailerSend notification to {}.", target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
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
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> MailerSend {
    MailerSend::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url))
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  /// Every request the mock server received at the MailerSend endpoint.
  async fn payloads(server: &MockServer) -> Vec<Value> {
    let requests = server.received_requests().await.unwrap();
    requests.iter().filter(|r| r.url.path() == "/v1/email").map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/email")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "mailersend://",
      "mailersend://:@/",
      "mailersend://abcd",
      "mailersend://abcd@host",
      "mailersend://bad+key*!:user@example.com",
      "mailersend://abcd:user@example.com/newuser@example.com?reply=%20!",
      "mailersend://user@host.com/to@example.com",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "mailersend://abcd:user@example.com",
      "mailersend://abcd:user@example.com/newuser@example.com",
      "mailersend://abcd:user@example.com/one@example.com/two@example.com",
      "mailersend://abcd:user@example.com/one@example.com?to=two@example.com",
      "mailersend://abcd:user@example.com/newuser@example.com?cc=cc@nuxref.com",
      "mailersend://abcd:user@example.com/newuser@example.com?bcc=bcc@nuxref.com",
      "mailersend://abcd:user@example.com?reply=reply@example.com",
      "mailersend://abcd:user@example.com/bademailaddress",
      "mailersend://abcd:user@example.com/newuser@example.com?cc=l2g@nuxref.com,!bad&bcc=l2g@nuxref.com,!bad",
      "mailersend://abcd:user@example.com?format=text",
      "mailersend://abcd:user@example.ca/newuser@example.ca",
      "mailersend://abcd:user@example.uk/newuser@example.uk",
      "mailersend://abcd:user@example.au/newuser@example.au",
      "mailersend://mlsn.abc123:sender@example.com/to@example.com",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values below come from upstream's parsed objects
    let obj = parse("mailersend://abcd:user@example.com");
    assert_eq!(obj.apikey, "abcd");
    assert_eq!(obj.from_email, "user@example.com");
    assert_eq!(obj.targets, vec!["user@example.com"]);

    assert_eq!(parse("mailersend://abcd:user@example.com/one@example.com?to=two@example.com").targets, vec!["one@example.com", "two@example.com"]);
    assert_eq!(parse("mailersend://abcd:user@example.com?reply=reply@example.com").reply_to.as_deref(), Some("reply@example.com"));
    assert!(parse("mailersend://abcd:user@example.com/bademailaddress").targets.is_empty());

    let obj = parse("mailersend://abcd:user@example.com/newuser@example.com?cc=l2g@nuxref.com,!bad&bcc=l2g@nuxref.com,!bad");
    assert_eq!(obj.cc, vec!["l2g@nuxref.com"]);
    assert_eq!(obj.bcc, vec!["l2g@nuxref.com"]);
  }

  #[test]
  fn test_service_details() {
    let details = MailerSend::static_details();
    assert_eq!(details.service_name, "MailerSend");
    assert_eq!(details.protocols, vec!["mailersend"]);
    assert!(details.attachment_support);
    let svc = from_url("mailersend://abcd:user@example.com").unwrap();
    assert_eq!(svc.notify_formats(), vec![NotifyFormat::Html, NotifyFormat::Text]);
    assert_eq!(svc.request_rate_per_sec(), 0.2);
  }

  #[tokio::test]
  async fn test_send_html() {
    let server = MockServer::start().await;
    // The apprise-go golden for this URL
    Mock::given(method("POST"))
      .and(path("/v1/email"))
      .and(header("Authorization", "Bearer apikey123"))
      .and(header("Content-Type", "application/json"))
      .and(header("Accept", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({
        "from": { "email": "sender@example.com" },
        "to": [{ "email": "target@example.com" }],
        "subject": "apprise parity",
        "html": "hello from python",
        "text": "hello from python",
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("mailersend://apikey123:sender@example.com/target@example.com").unwrap();
    let c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("apprise parity", "hello from python") };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_text_format() {
    let server = mock(202).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("mailersend://abcd:user@example.com/to@example.com?format=text"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("", "<p>body</p>"))).await.is_success());
    // Matches pycapture.py
    assert_eq!(
      payloads(&server).await,
      vec![json!({
        "from": { "email": "user@example.com" },
        "to": [{ "email": "to@example.com" }],
        "subject": "<no subject>",
        "text": "<p>body</p>",
        "html": "&lt;p&gt;body&lt;/p&gt;",
      })]
    );
  }

  #[tokio::test]
  async fn test_send_markdown_input() {
    let server = mock(202).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("mailersend://apikey123:sender@example.com/target@example.com"));
    let c = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("over", "**bold** and [link](https://example.com) plus `code`.") };
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&c)).await.is_success());
    assert_eq!(
      payloads(&server).await[0],
      json!({
        "from": { "email": "sender@example.com" },
        "to": [{ "email": "target@example.com" }],
        "subject": "over",
        "html": "<p><strong>bold</strong> and <a href=\"https://example.com\">link</a> plus <code>code</code>.</p>",
        "text": "bold and link plus\ncode\n.",
      })
    );
  }

  #[tokio::test]
  async fn test_send_cc_bcc_reply_attachments() {
    let server = mock(202).await;
    let svc = from_url(
      "mailersend://mlsn.abc123:user@example.com/to@example.com/b@example.com\
       ?cc=cc@example.com,to@example.com&bcc=bcc@example.com,to@example.com&reply=Joe<reply@example.com>",
    )
    .unwrap();
    let mut c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("t", "b") };
    c.attachments = vec![Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() }];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());

    // Matches pycapture.py: targets sorted, each removed from cc/bcc
    let attachments = json!([{ "content": "UERGREFUQQ==", "filename": "report.pdf", "disposition": "attachment" }]);
    assert_eq!(
      payloads(&server).await,
      vec![
        json!({
          "from": { "email": "user@example.com" }, "to": [{ "email": "b@example.com" }], "subject": "t", "html": "b", "text": "b",
          "attachments": attachments, "reply_to": { "email": "reply@example.com" },
          "cc": [{ "email": "cc@example.com" }], "bcc": [{ "email": "bcc@example.com" }, { "email": "to@example.com" }],
        }),
        json!({
          "from": { "email": "user@example.com" }, "to": [{ "email": "to@example.com" }], "subject": "t", "html": "b", "text": "b",
          "attachments": attachments, "reply_to": { "email": "reply@example.com" },
          "cc": [{ "email": "cc@example.com" }], "bcc": [{ "email": "bcc@example.com" }],
        }),
      ]
    );
  }

  #[tokio::test]
  async fn test_send_unnamed_attachment() {
    let server = mock(200).await;
    let svc = from_url("mailersend://abcd:user@example.com").unwrap();
    let mut c = ctx("t", "b");
    c.attachments = vec![Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "application/octet-stream".into() }];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
    assert_eq!(payloads(&server).await[0]["attachments"][0]["filename"], json!("file001.dat"));
  }

  #[tokio::test]
  async fn test_send_no_recipients() {
    let server = mock(202).await;
    let svc = from_url("mailersend://abcd:user@example.com/bademailaddress").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    assert!(payloads(&server).await.is_empty());
  }

  #[tokio::test]
  async fn test_send_failure() {
    // 201 is not one of the codes upstream accepts
    for status in [201, 401, 422, 500, 999] {
      let server = mock(status).await;
      let svc = from_url("mailersend://abcd:user@example.com/to@example.com").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap(), "status {}", status);
    }
  }

  #[tokio::test]
  async fn test_send_partial_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/email"))
      .and(body_json(json!({
        "from": { "email": "user@example.com" }, "to": [{ "email": "t2@example.com" }], "subject": "<no subject>", "text": "body", "html": "body",
      })))
      .respond_with(ResponseTemplate::new(500))
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/email")).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let svc = from_url("mailersend://abcd:user@example.com/t1@example.com/t2@example.com").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "body"))).await.unwrap());
    assert_eq!(payloads(&server).await.len(), 2);
  }

  #[tokio::test]
  async fn test_send_connection_error() {
    let svc = from_url("mailersend://abcd:user@example.com/to@example.com").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:9", svc.send(&ctx("t", "b"))).await.unwrap());
  }
}
