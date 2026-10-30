use crate::conversion::convert_between;
use crate::delivery::{is_delivered, mark_delivered, recall, remember};
use crate::error::NotifyError;
use crate::notify::apprise_api::uuid4;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::email::{formataddr, is_email, parse_emails_all, parse_list};
use crate::utils::parse::ParsedUrl;
use crate::utils::sanitize::sanitize_payload;
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use std::collections::HashMap;

/// Groundmail rejects an empty subject.
const DEFAULT_EMPTY_SUBJECT: &str = "<no subject>";

pub struct Groundmail {
  apikey: String,
  from_name: String,
  from_email: String,
  targets: Vec<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  reply_to: Vec<String>,
  /// Display names of the sender, cc and reply-to addresses (upstream
  /// `names`).
  names: HashMap<String, Option<String>>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream's API key regex `^[A-Z0-9._-]+$` (any case).
fn is_apikey(key: &str) -> bool {
  !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Python's `repr()` of a string, for the URL identifier.
fn py_repr(s: &str) -> String {
  let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
  let mut out = String::from(quote);
  for c in s.chars() {
    match c {
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      c if c == quote => {
        out.push('\\');
        out.push(c);
      }
      c => out.push(c),
    }
  }
  out.push(quote);
  out
}

/// Upstream `GROUNDMAIL_HTTP_ERROR_MAP` over the base HTTP lookup,
/// formatted for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    400 => "Check that the parameters were correct., ",
    401 => "The API key used was missing or invalid., ",
    403 => "The From address is not on a verified domain, or the recipient is not permitted for this sender., ",
    404 => "The resource was not found., ",
    405 => "Method not allowed., ",
    409 => "The request is already being processed., ",
    422 => "The recipient is suppressed or was rejected., ",
    429 => "The rate or sending limit was exceeded., ",
    500 => "Internal server error., ",
    502 => "The email could not be handed off for delivery., ",
    503 => "Email delivery is temporarily unavailable., ",
    _ => "",
  }
}

impl Groundmail {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // groundmail://{apikey}:{from_user}@{from_domain}/{targets}
    // groundmail://{apikey}@{target}?from={from}
    let apikey = url.get("apikey").filter(|k| !k.is_empty()).map(str::to_string).or_else(|| url.user.clone()).unwrap_or_default();
    if !is_apikey(&apikey) {
      tracing::warn!("An invalid Groundmail API Key was specified.");
      return None;
    }

    let host = url.host.clone().unwrap_or_default();
    let mut raw_targets: Vec<String> = Vec::new();
    let mut from = match url.get("from").filter(|f| !f.is_empty()) {
      Some(from) => {
        // The host is a recipient when ?from= names the sender
        if !host.is_empty() {
          raw_targets.push(host);
        }
        from.to_string()
      }
      None => format!("{}@{}", url.password.as_deref().or(url.user.as_deref()).unwrap_or_default(), host),
    };
    if let Some(name) = url.get("name").filter(|n| !n.is_empty()) {
      from = formataddr(Some(name), &from);
    }
    let Some(sender) = is_email(&from) else {
      tracing::warn!("Invalid ~From~ email specified: {}", from);
      return None;
    };

    let mut names: HashMap<String, Option<String>> = HashMap::new();
    names.insert(sender.full_email.clone(), sender.display_name());

    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_list(&[to]));
    }
    let raw_targets = parse_emails_all(&raw_targets);
    // Without any target the sender notifies itself
    let targets: Vec<String> = if raw_targets.is_empty() {
      vec![sender.full_email.clone()]
    } else {
      raw_targets
        .iter()
        .filter_map(|recipient| {
          let parsed = is_email(recipient);
          if parsed.is_none() {
            tracing::warn!("Dropped invalid email ({}) specified.", recipient);
          }
          parsed.map(|e| e.full_email)
        })
        .collect()
    };

    let mut collect = |raw: Vec<String>, kind: &str, named: bool| {
      let mut list: Vec<String> = Vec::new();
      for recipient in parse_emails_all(&raw) {
        match is_email(&recipient) {
          Some(e) => {
            if named {
              names.insert(e.full_email.clone(), e.display_name());
            }
            if !list.contains(&e.full_email) {
              list.push(e.full_email);
            }
          }
          None => tracing::warn!("Dropped invalid {} email ({}) specified.", kind, recipient),
        }
      }
      list
    };
    let list = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(|v| parse_list(&[v])).unwrap_or_default();
    let cc = collect(list("cc"), "Carbon Copy", true);
    let bcc = collect(list("bcc"), "Blind Carbon Copy", false);
    let reply_to = collect(url.get("reply").filter(|v| !v.is_empty()).map(|v| vec![v.to_string()]).unwrap_or_default(), "Reply To", true);

    Some(Self {
      apikey,
      from_name: sender.name,
      from_email: sender.full_email,
      targets,
      cc,
      bcc,
      reply_to,
      names,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Groundmail",
      service_url: Some("https://groundmail.dev"),
      setup_url: Some("https://appriseit.com/services/groundmail/"),
      protocols: vec!["groundmail"],
      description: "Send email via Groundmail.",
      attachment_support: true,
    }
  }

  fn named(&self, addr: &str) -> String {
    formataddr(self.names.get(addr).and_then(|n| n.as_deref()), addr)
  }
}

#[async_trait]
impl Notify for Groundmail {
  fn schemas(&self) -> &[&str] {
    &["groundmail"]
  }
  fn service_name(&self) -> &str {
    "Groundmail"
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
    // Free accounts may send 60 emails per minute
    1.0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    // Upstream hashes str() of its (name, address) tuple
    let from = format!("({}, {})", py_repr(&self.from_name), py_repr(&self.from_email));
    Some(vec!["groundmail".into(), self.apikey.clone(), from])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Groundmail email recipients to notify");
      return Ok(false);
    }

    let mut base = json!({
      "from": formataddr(Some(&self.from_name), &self.from_email),
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
            "name": if att.name.is_empty() { format!("file{:03}.dat", i + 1) } else { att.name.clone() },
            "content_type": att.mime_type,
            "data": base64::engine::general_purpose::STANDARD.encode(&att.data),
          })
        })
        .collect();
    }

    let url = format!("{}/emails", service_base("https://api.groundmail.dev"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for target in &self.targets {
      // A retry skips the targets that already accepted the message
      if is_delivered(target, false) {
        continue;
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

      // The key survives retries of this notification, so a lost response
      // to an accepted email does not send it twice
      let memo_key = format!("idempotency:{}", target);
      let idempotency_key = match recall(&memo_key, false).and_then(|v| v.as_str().map(str::to_string)) {
        Some(key) => key,
        None => {
          let key = uuid4();
          remember(&memo_key, json!(key), false);
          key
        }
      };

      if tracing::enabled!(tracing::Level::DEBUG) {
        tracing::debug!("Groundmail POST URL: {} (cert_verify={})", url, self.verify_certificate);
        tracing::debug!("Groundmail Payload: {}", sanitize_payload(&payload));
      }

      throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", self.apikey))
        .header("Idempotency-Key", &idempotency_key)
        .body(payload.to_string())
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200..=202) => {
          tracing::info!("Sent Groundmail notification to {}.", target);
          mark_delivered(target, false);
        }
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send Groundmail notification to {}: {}error={}.", target, status_message(status), status);
          tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Groundmail notification to {}.", target);
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
  use crate::utils::plugin_args::is_uuid4;
  use serde_json::Value;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const APIKEY: &str = "gm_live_abc123def456abc123def456abc123";

  fn parse(url: &str) -> Groundmail {
    Groundmail::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url))
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/emails"))
      .respond_with(ResponseTemplate::new(status).set_body_string(r#"{"id":"msg_794399cdfb1bfc21","status":"sent"}"#))
      .mount(&server)
      .await;
    server
  }

  /// Every request the mock server received at the Groundmail endpoint.
  async fn requests(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() == "/emails").collect()
  }

  async fn payloads(server: &MockServer) -> Vec<Value> {
    requests(server).await.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  fn idempotency_key(request: &wiremock::Request) -> String {
    request.headers.get("idempotency-key").unwrap().to_str().unwrap().to_string()
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["groundmail://", "groundmail://:@/", "groundmail://abcd", "groundmail://abcd@host", "groundmail://invalid-api-key+*-d:user@example.com"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "groundmail://abcd:user@example.com".to_string(),
      format!("groundmail://{}:user@example.com/newuser1@example.com", APIKEY),
      "groundmail://abcd:user@example.com/newuser2@example.com?name=Jessica".into(),
      "groundmail://abcd@newuser4%40example.com?name=Ralph&from=user2@example.ca".into(),
      "groundmail://?apikey=abcd&from=Joe<user@example.com>&to=newuser5@example.com".into(),
      "groundmail://?apikey=abcd&from=Joe<user@example.com>&reply=John<newuser6@example.com>".into(),
      "groundmail://?apikey=abcd&from=Joe<user@example.com>&reply=garbage%".into(),
      "groundmail://abcd:user@example.com/newuser7@example.com?bcc=l2g@nuxref.com".into(),
      "groundmail://abcd:user@example.com/newuser8@example.com?cc=l2g@nuxref.com".into(),
      "groundmail://abcd:user@example.com/newuser8@example.com?cc=Chris<l2g@nuxref.com>".into(),
      "groundmail://abcd:user@example.com/newuser9@example.com?to=l2g@nuxref.com".into(),
      "groundmail://abcd:user@example.com?format=text".into(),
      "groundmail://abcd:user@example.ca/newuser0@example.ca".into(),
      "groundmail://abcd:user@example.uk/newuser01@example.uk".into(),
      "groundmail://abcd:user@example.au/newuser02@example.au".into(),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values below come from upstream's parsed objects
    let obj = parse("groundmail://abcd:user@example.com");
    assert_eq!((obj.apikey.as_str(), obj.from_name.as_str(), obj.from_email.as_str()), ("abcd", "", "user@example.com"));
    assert_eq!(obj.targets, vec!["user@example.com"]);

    let obj = parse("groundmail://abcd:user@example.com/newuser2@example.com?name=Jessica");
    assert_eq!((obj.from_name.as_str(), obj.from_email.as_str()), ("Jessica", "user@example.com"));
    assert_eq!(obj.targets, vec!["newuser2@example.com"]);

    let obj = parse("groundmail://abcd@newuser4%40example.com?name=Ralph&from=user2@example.ca");
    assert_eq!((obj.from_name.as_str(), obj.from_email.as_str()), ("Ralph", "user2@example.ca"));
    assert_eq!(obj.targets, vec!["newuser4@example.com"]);

    let obj = parse("groundmail://?apikey=abcd&from=Joe<user@example.com>&reply=John<newuser6@example.com>");
    assert_eq!((obj.apikey.as_str(), obj.from_name.as_str()), ("abcd", "Joe"));
    assert_eq!(obj.targets, vec!["user@example.com"]);
    assert_eq!(obj.reply_to, vec!["newuser6@example.com"]);
    assert_eq!(obj.names.get("newuser6@example.com"), Some(&Some("John".to_string())));

    assert!(parse("groundmail://?apikey=abcd&from=Joe<user@example.com>&reply=garbage%").reply_to.is_empty());

    let obj = parse("groundmail://abcd:user@example.com/newuser8@example.com?cc=Chris<l2g@nuxref.com>");
    assert_eq!(obj.cc, vec!["l2g@nuxref.com"]);
    assert_eq!(obj.names.get("l2g@nuxref.com"), Some(&Some("Chris".to_string())));

    // Path targets keep their order; ?to= follows
    assert_eq!(parse("groundmail://abcd:user@example.com/newuser9@example.com?to=l2g@nuxref.com").targets, vec!["newuser9@example.com", "l2g@nuxref.com"]);

    // Every invalid target dropped leaves nobody to notify
    assert!(parse("groundmail://abcd:user@example.com/!invalid").targets.is_empty());

    // str() of upstream's (name, address) tuple
    let obj = parse("groundmail://abcd:user@example.com/newuser2@example.com?name=Jessica");
    assert_eq!(obj.url_identifier().unwrap(), vec!["groundmail", "abcd", "('Jessica', 'user@example.com')"]);
    assert_eq!(parse("groundmail://abcd:user@example.com").url_identifier().unwrap()[2], "('', 'user@example.com')");
    assert_eq!(py_repr("O'Neil"), "\"O'Neil\"");
  }

  #[test]
  fn test_service_details() {
    let details = Groundmail::static_details();
    assert_eq!(details.service_name, "Groundmail");
    assert_eq!(details.protocols, vec!["groundmail"]);
    assert!(details.attachment_support);
    let svc = from_url("groundmail://abcd:user@example.com").unwrap();
    assert_eq!(svc.notify_formats(), vec![NotifyFormat::Html, NotifyFormat::Text]);
    assert_eq!(svc.request_rate_per_sec(), 1.0);
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/emails"))
      .and(header("Authorization", format!("Bearer {}", APIKEY).as_str()))
      .and(header("Content-Type", "application/json"))
      .and(header("Accept", "application/json"))
      .and(header("User-Agent", APP_ID))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!(
      "groundmail://{}:user@example.com/to@example.com?name=Apprise%20Bot&cc=Chris<cc@example.com>,to@example.com\
       &bcc=bcc@example.com,to@example.com&reply=Help<reply@example.com>",
      APIKEY
    ))
    .unwrap();
    let c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("title", "<p>body</p>") };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());

    // Matches pycapture.py
    let sent = requests(&server).await;
    assert!(is_uuid4(&idempotency_key(&sent[0])));
    assert_eq!(
      serde_json::from_slice::<Value>(&sent[0].body).unwrap(),
      json!({
        "from": "Apprise Bot <user@example.com>",
        "subject": "title",
        "html": "<p>body</p>",
        "text": "body",
        "to": "to@example.com",
        "cc": ["Chris <cc@example.com>"],
        "bcc": ["bcc@example.com"],
        "reply_to": ["Help <reply@example.com>"],
      })
    );
  }

  #[tokio::test]
  async fn test_send_text_format() {
    let server = mock(200).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("groundmail://abcd:user@example.com/to@example.com?format=text"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("", "line1\nline2"))).await.is_success());
    // Matches pycapture.py
    assert_eq!(
      payloads(&server).await,
      vec![json!({
        "from": "user@example.com",
        "subject": "<no subject>",
        "text": "line1\nline2",
        "html": "line1<br/>line2",
        "to": "to@example.com",
      })]
    );
  }

  #[tokio::test]
  async fn test_send_markdown_input() {
    let server = mock(200).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("groundmail://abcd:user@example.com/to@example.com"));
    let c = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("over", "**bold** and [link](https://example.com) plus `code`.") };
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&c)).await.is_success());
    let sent = payloads(&server).await;
    assert_eq!(sent[0]["html"], json!("<p><strong>bold</strong> and <a href=\"https://example.com\">link</a> plus <code>code</code>.</p>"));
    assert_eq!(sent[0]["text"], json!("bold and link plus\ncode\n."));
  }

  #[tokio::test]
  async fn test_send_per_target_keys_and_attachments() {
    let server = mock(201).await;
    let svc = from_url("groundmail://abcd:user@example.com/t1@example.com/t2@example.com").unwrap();
    let mut c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("title", "body") };
    c.attachments = vec![
      Attachment { name: "apprise-test.gif".into(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() },
      Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "application/octet-stream".into() },
    ];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());

    let sent = requests(&server).await;
    assert_eq!(sent.len(), 2);
    assert_ne!(idempotency_key(&sent[0]), idempotency_key(&sent[1]));
    let payloads: Vec<Value> = sent.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(payloads[0]["to"], json!("t1@example.com"));
    assert_eq!(payloads[1]["to"], json!("t2@example.com"));
    assert_eq!(
      payloads[0]["attachments"],
      json!([
        { "name": "apprise-test.gif", "content_type": "image/gif", "data": "R0lGODlh" },
        { "name": "file002.dat", "content_type": "application/octet-stream", "data": "eA==" },
      ])
    );
  }

  #[tokio::test]
  async fn test_retry_keeps_idempotency_key() {
    let server = MockServer::start().await;
    // t2 fails once with a 503, then is accepted
    Mock::given(method("POST"))
      .and(path("/emails"))
      .and(wiremock::matchers::body_partial_json(json!({ "to": "t2@example.com" })))
      .respond_with(ResponseTemplate::new(503))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/emails")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("groundmail://abcd:user@example.com/t1@example.com/t2@example.com?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("t", "body"))).await.is_success());

    let sent = requests(&server).await;
    let to: Vec<Value> = sent.iter().map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["to"].clone()).collect();
    assert_eq!(to, vec![json!("t1@example.com"), json!("t2@example.com"), json!("t2@example.com")]);
    assert_eq!(idempotency_key(&sent[1]), idempotency_key(&sent[2]));
    assert_ne!(idempotency_key(&sent[0]), idempotency_key(&sent[1]));
  }

  #[tokio::test]
  async fn test_send_no_recipients() {
    let server = mock(200).await;
    let svc = from_url("groundmail://abcd:user@example.com/!invalid").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "body"))).await.unwrap());
    assert!(requests(&server).await.is_empty());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [400, 403, 500, 999] {
      let server = mock(status).await;
      let svc = from_url("groundmail://abcd:user@example.ca/newuser0@example.ca").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap(), "status {}", status);
    }
    let svc = from_url("groundmail://abcd:user@example.au/newuser02@example.au").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:9", svc.send(&ctx("t", "b"))).await.unwrap());
  }
}
