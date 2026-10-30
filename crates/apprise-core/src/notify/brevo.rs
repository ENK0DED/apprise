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

/// Brevo rejects an empty subject.
const DEFAULT_EMPTY_SUBJECT: &str = "<no subject>";

/// Upstream `BREVO_VALID_EXTENSIONS`: Brevo only accepts attachments whose
/// file name ends in one of these.
const BREVO_VALID_EXTENSIONS: &[&str] = &[
  "xlsx", "xls", "ods", "docx", "docm", "doc", "csv", "pdf", "txt", "rtf", "msg", "pub", "mobi", "ppt", "pptx", "eps", "odt", "ics", "xml", "css", "html",
  "htm", "shtml", "gif", "jpg", "jpeg", "png", "tif", "tiff", "bmp", "cgm", "zip", "tar", "ez", "pkpass", "mp3", "m4a", "m4v", "wma", "ogg", "flac", "wav",
  "aif", "aifc", "aiff", "mp4", "mov", "avi", "mkv", "mpeg", "mpg", "wmv",
];

pub struct Brevo {
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
      None => tracing::warn!("Dropped invalid {}email ({}) specified.", kind, entry),
    }
  }
  out
}

impl Brevo {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // brevo://{apikey}:{from_user}@{from_domain}/{targets}
    let host = url.verified_host()?;
    let apikey = url.user.as_deref()?;
    let from_user = url.password.as_deref()?;

    if !is_apikey(apikey) {
      tracing::warn!("An invalid Brevo API Key was specified.");
      return None;
    }

    let from = format!("{}@{}", from_user, host);
    let Some(sender) = is_email(&from) else {
      tracing::warn!("Invalid ~From~ email specified: {}", from);
      return None;
    };

    let reply_to = match url.get("reply").filter(|r| !r.is_empty()) {
      Some(reply) => match is_email(reply) {
        Some(e) => Some(e.full_email),
        None => {
          tracing::warn!("An invalid Brevo Reply To ({}) was specified.", reply);
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
            tracing::warn!("Dropped invalid email ({}) specified.", recipient);
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
      service_name: "Brevo",
      service_url: Some("https://www.brevo.com/"),
      setup_url: Some("https://appriseit.com/services/brevo/"),
      protocols: vec!["brevo"],
      description: "Send email via Brevo (formerly Sendinblue).",
      attachment_support: true,
    }
  }
}

/// The name Brevo receives for an attachment: Brevo decides what it accepts
/// from the file extension alone, so anything else gets `.txt` appended.
fn safe_attachment_name(name: &str, no: usize) -> String {
  let raw = if name.is_empty() { format!("file{:03}.txt", no) } else { name.to_string() };
  let valid = py_extension(&raw).is_some_and(|ext| BREVO_VALID_EXTENSIONS.contains(&ext.to_lowercase().as_str()));
  if valid { raw } else { format!("{}.txt", raw) }
}

/// The extension Python's `os.path.splitext()` finds, without its dot. A
/// file name made only of leading dots (`.bashrc`) has none.
fn py_extension(name: &str) -> Option<&str> {
  let base = &name[name.rfind('/').map_or(0, |i| i + 1)..];
  let dot = base.rfind('.')?;
  base[..dot].chars().any(|c| c != '.').then(|| &base[dot + 1..])
}

/// Upstream `BREVO_HTTP_ERROR_MAP` over the base HTTP lookup, formatted for
/// the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Invalid payload or missing parameters., ",
    401 => "Unauthorized - Invalid Brevo API key., ",
    402 => "Payment Required - Plan limitation or credit issue., ",
    404 => "Page not found., ",
    405 => "Method not allowed., ",
    429 => "Too Many Requests - Rate limit exceeded., ",
    500 => "Internal server error., ",
    503 => "Servers are overloaded., ",
    _ => "",
  }
}

#[async_trait]
impl Notify for Brevo {
  fn schemas(&self) -> &[&str] {
    &["brevo"]
  }
  fn service_name(&self) -> &str {
    "Brevo"
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
    Some(vec!["brevo".into(), self.apikey.clone(), self.from_email.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Brevo email recipients to notify");
      return Ok(false);
    }

    let mut base = json!({
      "sender": { "email": self.from_email },
      "subject": if ctx.title.is_empty() { DEFAULT_EMPTY_SUBJECT } else { ctx.title.as_str() },
    });
    // Brevo wants both parts; the other one is derived from the body
    if ctx.body_format == NotifyFormat::Text {
      base["textContent"] = json!(ctx.body);
      base["htmlContent"] = json!(convert_between(&NotifyFormat::Text, &NotifyFormat::Html, &ctx.body));
    } else {
      base["htmlContent"] = json!(ctx.body);
      base["textContent"] = json!(convert_between(&NotifyFormat::Html, &NotifyFormat::Text, &ctx.body));
    }
    if !ctx.attachments.is_empty() {
      base["attachment"] = ctx
        .attachments
        .iter()
        .enumerate()
        .map(|(i, att)| {
          json!({
            "content": base64::engine::general_purpose::STANDARD.encode(&att.data),
            "name": safe_attachment_name(&att.name, i + 1),
          })
        })
        .collect();
    }
    if let Some(reply_to) = &self.reply_to {
      base["replyTo"] = json!({ "email": reply_to });
    }

    let url = format!("{}/v3/smtp/email", service_base("https://api.brevo.com"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for target in &self.targets {
      // A retry skips the targets that already accepted the message
      if is_delivered(target, false) {
        continue;
      }

      // To, cc and bcc must not overlap or Brevo refuses the message
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
        tracing::debug!("Brevo POST URL: {} (cert_verify={})", url, self.verify_certificate);
        tracing::debug!("Brevo Payload: {}", sanitize_payload(&payload));
      }

      throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("api-key", &self.apikey)
        .body(payload.to_string())
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 201 | 202) => {
          tracing::info!("Sent Brevo notification to {}.", target);
          mark_delivered(target, false);
        }
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send Brevo notification to {}: {}error={}.", target, status_message(status), status);
          tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Brevo notification to {}.", target);
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

  fn parse(url: &str) -> Brevo {
    Brevo::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url))
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  /// Every request the mock server received at the Brevo endpoint.
  async fn payloads(server: &MockServer) -> Vec<Value> {
    let requests = server.received_requests().await.unwrap();
    requests.iter().filter(|r| r.url.path() == "/v3/smtp/email").map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "brevo://",
      "brevo://:@/",
      "brevo://abcd",
      "brevo://abcd@host",
      "brevo://invalid-api-key+*-d:user@example.com",
      "brevo://abcd:user@example.com/newuser@example.com?reply=%20!",
      // The host must be a valid hostname
      "brevo://abcd:user@exa%20mple.com",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "brevo://abcd:user@example.com?format=text",
      "brevo://abcd:user@example.com/newuser@example.com?reply=user@example.ca",
      "brevo://abcd:user@example.com/bademailaddress",
      "brevo://abcd:user@example.com/newuser@example.com?bcc=l2g@nuxref.com",
      "brevo://abcd:user@example.com/newuser@example.com?cc=l2g@nuxref.com",
      "brevo://abcd:user@example.com/newuser@example.com?to=l2g@nuxref.com",
      "brevo://abcd:user@example.ca/newuser@example.ca",
      "brevo://abcd:user@example.uk/newuser@example.uk",
      "brevo://abcd:user@example.au/newuser@example.au",
      // Periods are allowed in the API key
      "brevo://xkeysib-a.b_c:user@example.com",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values below come from upstream's parsed objects
    let obj = parse("brevo://abcd:user@example.com?format=text");
    assert_eq!(obj.apikey, "abcd");
    assert_eq!(obj.from_email, "user@example.com");
    assert_eq!(obj.targets, vec!["user@example.com"]);
    assert_eq!(obj.reply_to, None);

    let obj = parse("brevo://abcd:user@example.com/newuser@example.com?reply=Chris<user@example.ca>");
    assert_eq!(obj.targets, vec!["newuser@example.com"]);
    assert_eq!(obj.reply_to.as_deref(), Some("user@example.ca"));

    // Every invalid target dropped leaves nobody to notify
    assert!(parse("brevo://abcd:user@example.com/bademailaddress").targets.is_empty());

    // Targets are sorted and unique, as upstream's parse_list() returns them
    let obj = parse("brevo://abcd:user@example.com/newuser@example.com/b@example.com?to=l2g@nuxref.com,b@example.com");
    assert_eq!(obj.targets, vec!["b@example.com", "l2g@nuxref.com", "newuser@example.com"]);

    let obj = parse("brevo://abcd:user@example.com?cc=Chris<abc@test.org>,!invalid,abc@test.org&bcc=abc@def.com,!invalid");
    assert_eq!(obj.cc, vec!["abc@test.org"]);
    assert_eq!(obj.bcc, vec!["abc@def.com"]);
  }

  #[test]
  fn test_attachment_names() {
    assert_eq!(safe_attachment_name("report.pdf", 1), "report.pdf");
    assert_eq!(safe_attachment_name("image.PNG", 1), "image.PNG");
    assert_eq!(safe_attachment_name("archive.tar.gz", 1), "archive.tar.gz.txt");
    assert_eq!(safe_attachment_name("noext", 1), "noext.txt");
    assert_eq!(safe_attachment_name(".bashrc", 1), ".bashrc.txt");
    assert_eq!(safe_attachment_name("trailing.", 1), "trailing..txt");
    assert_eq!(safe_attachment_name("data.dat", 2), "data.dat.txt");
    assert_eq!(safe_attachment_name("", 3), "file003.txt");
  }

  #[test]
  fn test_service_details() {
    let details = Brevo::static_details();
    assert_eq!(details.service_name, "Brevo");
    assert_eq!(details.protocols, vec!["brevo"]);
    assert!(details.attachment_support);
    let svc = from_url("brevo://abcd:user@example.com").unwrap();
    assert_eq!(svc.notify_formats(), vec![NotifyFormat::Html, NotifyFormat::Text]);
    assert_eq!(svc.request_rate_per_sec(), 0.2);
  }

  #[tokio::test]
  async fn test_send_text() {
    let server = MockServer::start().await;
    // Matches pycapture.py for this URL with --input-format text
    Mock::given(method("POST"))
      .and(path("/v3/smtp/email"))
      .and(header("api-key", "apikey"))
      .and(header("Content-Type", "application/json"))
      .and(header("Accept", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({
        "sender": { "email": "sender@domain.com" },
        "to": [{ "email": "recipient@domain.com" }],
        "subject": "apprise parity",
        "textContent": "hello from python",
        "htmlContent": "hello&nbsp;from&nbsp;python",
      })))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("brevo://apikey:sender@domain.com/recipient@domain.com").unwrap();
    let mut c = ctx("apprise parity", "hello from python");
    c.body_format = NotifyFormat::Text;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_markdown_input() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v3/smtp/email")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("brevo://apikey:sender@domain.com/recipient@domain.com"));
    let c = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("over", "**bold** and [link](https://example.com) plus `code`.") };
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&c)).await.is_success());

    // Matches the apprise-go golden captured from upstream
    assert_eq!(
      payloads(&server).await,
      vec![json!({
        "sender": { "email": "sender@domain.com" },
        "to": [{ "email": "recipient@domain.com" }],
        "subject": "over",
        "htmlContent": "<p><strong>bold</strong> and <a href=\"https://example.com\">link</a> plus <code>code</code>.</p>",
        "textContent": "bold and link plus\ncode\n.",
      })]
    );
  }

  #[tokio::test]
  async fn test_send_html_fallback_subject_and_self_target() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v3/smtp/email")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let svc = from_url("brevo://apikey:sender@domain.com").unwrap();
    let c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("", "<b>hello</b>") };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
    assert_eq!(
      payloads(&server).await,
      vec![json!({
        "sender": { "email": "sender@domain.com" },
        "to": [{ "email": "sender@domain.com" }],
        "subject": "<no subject>",
        "htmlContent": "<b>hello</b>",
        "textContent": "hello",
      })]
    );
  }

  #[tokio::test]
  async fn test_send_cc_bcc_reply_per_target() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v3/smtp/email")).respond_with(ResponseTemplate::new(202)).mount(&server).await;

    // Verified against pycapture.py: one request per target, the target
    // removed from cc/bcc and bcc entries removed from cc
    let svc = from_url(
      "brevo://abcd:user@example.com/a@example.com/b@example.com\
       ?cc=Chris<b@example.com>,c@example.com,d@example.com&bcc=d@example.com&reply=Joe<reply@example.com>",
    )
    .unwrap();
    let c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("t", "b") };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
    let sent = payloads(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["to"], json!([{ "email": "a@example.com" }]));
    assert_eq!(sent[0]["cc"], json!([{ "email": "b@example.com" }, { "email": "c@example.com" }]));
    assert_eq!(sent[0]["bcc"], json!([{ "email": "d@example.com" }]));
    assert_eq!(sent[0]["replyTo"], json!({ "email": "reply@example.com" }));
    assert_eq!(sent[1]["to"], json!([{ "email": "b@example.com" }]));
    assert_eq!(sent[1]["cc"], json!([{ "email": "c@example.com" }]));
    assert_eq!(sent[1]["bcc"], json!([{ "email": "d@example.com" }]));
  }

  #[tokio::test]
  async fn test_send_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v3/smtp/email")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let svc = from_url("brevo://key:sender@example.com/user@example.com").unwrap();
    let mut c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("title", "body") };
    c.attachments = vec![
      Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() },
      Attachment { name: "notes.dat".into(), data: b"x".to_vec(), mime_type: "application/octet-stream".into() },
    ];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
    // The apprise-go golden: {"content": "UERGREFUQQ==", "name": "report.pdf"}
    assert_eq!(
      payloads(&server).await[0]["attachment"],
      json!([{ "content": "UERGREFUQQ==", "name": "report.pdf" }, { "content": "eA==", "name": "notes.dat.txt" }])
    );
  }

  #[tokio::test]
  async fn test_send_no_recipients() {
    let server = MockServer::start().await;
    let svc = from_url("brevo://abcd:user@example.com/bademailaddress").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    assert!(payloads(&server).await.is_empty());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [400, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v3/smtp/email")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
      let svc = from_url("brevo://abcd:user@example.ca/newuser@example.ca").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap(), "status {}", status);
    }
  }

  #[tokio::test]
  async fn test_send_connection_error() {
    // Nothing listens on this port
    let svc = from_url("brevo://abcd:user@example.au/newuser@example.au").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:9", svc.send(&ctx("t", "b"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second target fails once, then succeeds
    Mock::given(method("POST"))
      .and(path("/v3/smtp/email"))
      .and(body_json(json!({
        "sender": { "email": "user@example.com" },
        "to": [{ "email": "b@example.com" }],
        "subject": "t",
        "htmlContent": "b",
        "textContent": "b",
      })))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v3/smtp/email")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("brevo://abcd:user@example.com/a@example.com/b@example.com?retry=1&wait=0"));
    let c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("t", "b") };
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&c)).await.is_success());
    let to: Vec<Value> = payloads(&server).await.iter().map(|p| p["to"][0]["email"].clone()).collect();
    assert_eq!(to, vec![json!("a@example.com"), json!("b@example.com"), json!("b@example.com")]);
  }
}
