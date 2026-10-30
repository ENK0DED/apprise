use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{formataddr, is_email, parse_emails, parse_emails_all, valid_addresses};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::qsd::{Qsd, unquote, validate_token};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// Recipients per request in batch mode.
const BATCH_SIZE: usize = 2000;

/// Pause between requests (upstream `request_rate_per_sec`).
const THROTTLE: Duration = Duration::from_millis(200);

/// Wait after a 429 before trying again.
const RETRY_WAIT: Duration = Duration::from_secs(5);

/// Attempts made while SparkPost keeps answering 429.
const RETRY_ATTEMPTS: u32 = 3;

pub struct SparkPost {
  apikey: String,
  from_name: Option<String>,
  from_email: String,
  /// `(display name, address)`
  targets: Vec<(Option<String>, String)>,
  cc: Vec<String>,
  bcc: Vec<String>,
  names: HashMap<String, Option<String>>,
  /// `+name=value` arguments, sent as email headers.
  headers: Vec<(String, String)>,
  /// `:name=value` arguments, sent as `substitution_data`.
  tokens: Vec<(String, String)>,
  region: String,
  batch: bool,
  throttle: Duration,
  retry_wait: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl SparkPost {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let qsd = Qsd::from_url(url);

    // The first path entry is the API key; the rest are recipients
    let mut path = url.path_parts.iter();
    let apikey = path.next().and_then(|k| validate_token(k))?;
    let user = url.user.clone().filter(|u| !u.is_empty())?;

    let region = match qsd.value("region") {
      Some(r) => unquote(r).to_lowercase(),
      None => "us".to_string(),
    };
    if region != "us" && region != "eu" {
      tracing::warn!("The SparkPost region specified ({}) is invalid.", region);
      return None;
    }

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

    let unquote_pair = |(k, v): (String, String)| (unquote(&k), unquote(&v));
    Some(Self {
      apikey,
      from_name,
      from_email,
      targets,
      cc,
      bcc,
      names,
      headers: qsd.prefixed('+').into_iter().map(unquote_pair).collect(),
      tokens: qsd.prefixed(':').into_iter().map(unquote_pair).collect(),
      region,
      batch: qsd.get("batch").map(parse_bool).unwrap_or(false),
      throttle: THROTTLE,
      retry_wait: RETRY_WAIT,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SparkPost",
      service_url: Some("https://sparkpost.com/"),
      setup_url: Some("https://appriseit.com/services/sparkpost/"),
      protocols: vec!["sparkpost"],
      description: "Send email via SparkPost.",
      attachment_support: true,
    }
  }

  fn api_base(&self) -> String {
    service_base(if self.region == "eu" { "https://api.eu.sparkpost.com" } else { "https://api.sparkpost.com" })
  }

  /// POST one transmission, retrying while SparkPost rate-limits us (429).
  async fn post(&self, client: &reqwest::Client, url: &str, payload: &Value, dest: &str) -> bool {
    let body = payload.to_string();
    for attempt in 1..=RETRY_ATTEMPTS {
      let result = client
        .post(url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Authorization", &self.apikey)
        .body(body.clone())
        .send()
        .await;
      match result {
        Ok(resp) if resp.status().as_u16() == 200 => {
          tracing::info!("Sent SparkPost notification to {}.", dest);
          return true;
        }
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send SparkPost notification to {}: {}error={}.", dest, status_message(status), status);
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          if status != 429 || attempt == RETRY_ATTEMPTS {
            return false;
          }
          tokio::time::sleep(self.retry_wait).await;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending SparkPost notification");
          tracing::debug!("Socket Exception: {}", e);
          return false;
        }
      }
    }
    false
  }
}

#[async_trait]
impl Notify for SparkPost {
  fn schemas(&self) -> &[&str] {
    &["sparkpost"]
  }
  fn service_name(&self) -> &str {
    "SparkPost"
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
      tracing::warn!("There are no SparkPost Email recipients to notify");
      return Ok(false);
    }

    let from_name = self.from_name.as_deref().filter(|n| !n.is_empty());
    let mut content = json!({
      "from": { "name": from_name.unwrap_or(&ctx.asset.app_desc), "email": self.from_email },
      // SparkPost refuses an empty or blank subject
      "subject": if ctx.title.trim().is_empty() { "." } else { ctx.title.as_str() },
      "reply_to": formataddr(from_name, &self.from_email),
    });
    content[if ctx.body_format == NotifyFormat::Html { "html" } else { "text" }] = json!(ctx.body);
    if !ctx.attachments.is_empty() {
      content["attachments"] = ctx
        .attachments
        .iter()
        .enumerate()
        .map(|(no, att)| {
          json!({
            "name": if att.name.is_empty() { format!("file{:03}.dat", no + 1) } else { att.name.clone() },
            "type": att.mime_type,
            "data": base64::engine::general_purpose::STANDARD.encode(&att.data),
          })
        })
        .collect();
    }
    let substitution_data: Map<String, Value> = self.tokens.iter().map(|(k, v)| (k.clone(), json!(v))).collect();

    let url = format!("{}/api/v1/transmissions/", self.api_base());
    let client = build_client(self.verify_certificate)?;
    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for (index, batch) in self.targets.chunks(batch_size).enumerate() {
      if index > 0 {
        tokio::time::sleep(self.throttle).await;
      }
      let in_batch = |addr: &String| batch.iter().any(|(_, t)| t == addr);
      let header_to = &batch[0].1;
      let mut recipients: Vec<Value> = Vec::new();
      for (name, addr) in batch {
        let mut address = json!({ "email": addr });
        if let Some(name) = name {
          address["name"] = json!(name);
        }
        recipients.push(json!({ "address": address }));
      }

      // Copies go out as extra recipients shown against the first To
      let mut headers: Map<String, Value> = self.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
      let cc: Vec<&String> = self.cc.iter().filter(|a| !self.bcc.contains(a) && !in_batch(a)).collect();
      for addr in &cc {
        let mut address = json!({ "email": addr, "header_to": header_to });
        if let Some(Some(name)) = self.names.get(*addr) {
          address["name"] = json!(name);
        }
        recipients.push(json!({ "address": address }));
      }
      if !cc.is_empty() {
        headers.insert("CC".into(), json!(cc.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(",")));
      }
      for addr in self.bcc.iter().filter(|a| !in_batch(a)) {
        recipients.push(json!({ "address": { "email": addr, "header_to": header_to } }));
      }

      let mut content = content.clone();
      if !headers.is_empty() {
        content["headers"] = Value::Object(headers);
      }
      let payload = json!({
        "options": { "open_tracking": false, "click_tracking": false },
        "content": content,
        "substitution_data": substitution_data,
        "recipients": recipients,
      });

      let dest = if recipients.len() <= 3 {
        recipients.iter().filter_map(|r| r["address"]["email"].as_str()).collect::<Vec<_>>().join(", ")
      } else {
        format!("{} recipients", recipients.len())
      };
      if !self.post(&client, &url, &payload, &dest).await {
        has_error = true;
      }
    }
    Ok(!has_error)
  }
}

/// Upstream `SPARKPOST_HTTP_ERROR_MAP`, formatted for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    400 => "A bad request was made to the server, ",
    401 => "Invalid User ID and/or Unauthorized User, ",
    403 => "Permission Denied; the provided API Key was not valid, ",
    404 => "There is a problem with the server query URI., ",
    405 => "Invalid HTTP method, ",
    420 => "Sending limit reached., ",
    422 => "Invalid data/format/type/length, ",
    429 => "To many requests per sec; rate limit, ",
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

  fn parse(url: &str) -> SparkPost {
    let mut sp = SparkPost::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url));
    sp.throttle = Duration::ZERO;
    sp.retry_wait = Duration::ZERO;
    sp
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(status).set_body_string(r#"{"results":{"id":"1"}}"#)).mount(&server).await;
    server
  }

  async fn bodies(server: &MockServer) -> Vec<Value> {
    server.received_requests().await.unwrap().iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  fn ctx(title: &str, body_format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: "body".into(), body_format, ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "sparkpost://".to_string(),
      "sparkpost://:@/".to_string(),
      "sparkpost://user@localhost.localdomain".to_string(),
      format!("sparkpost://localhost.localdomain/{}", "a".repeat(32)),
      format!("sparkpost://\"@localhost.localdomain/{}", "b".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?region=invalid", "a".repeat(32)),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("sparkpost://user@localhost.localdomain/{}", "c".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?format=markdown", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?format=html", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?format=text", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?region=uS", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?region=EU", "e".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?+X-Customer-Campaign-ID=Apprise", "f".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?:name=Chris&:status=admin", "g".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?bcc=user@example.com&cc=user2@example.com", "h".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}/test@example.com", "a".repeat(32)),
      // Loads, but has no valid recipient
      format!("sparkpost://user@localhost.localdomain/{}/invalid", "i".repeat(32)),
      format!(
        "sparkpost://user@example.com/{}/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com",
        "j".repeat(32)
      ),
      format!("sparkpost://user@localhost.localdomain/{}?to=test@example.com", "k".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}/test@example.com?name=\"Frodo\"", "l".repeat(32)),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let sp = parse(&format!(
      "sparkpost://user@example.com/{}/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com&region=EU&batch=yes&name=Sender",
      "j".repeat(32)
    ));
    assert_eq!(sp.apikey, "j".repeat(32));
    assert_eq!((sp.region.as_str(), sp.batch), ("eu", true));
    assert_eq!(sp.from_name.as_deref(), Some("Sender"));
    assert_eq!(sp.targets, vec![(None, "user1@example.com".to_string()), (Some("User2".to_string()), "user2@example.com".to_string())]);
    assert_eq!(sp.cc, vec!["user4@example.com", "user5@example.com"]);
    assert_eq!(sp.bcc, vec!["user3@example.com", "user1@example.com"]);
    assert_eq!(sp.names.get("user5@example.com"), Some(&Some("Da".to_string())));

    // Without a target the (named) sender is the recipient
    let sp = parse("sparkpost://user@example.com/key?name=Sender");
    assert_eq!(sp.targets, vec![(Some("Sender".to_string()), "user@example.com".to_string())]);
  }

  #[test]
  fn test_static_details() {
    let details = SparkPost::static_details();
    assert_eq!(details.service_name, "SparkPost");
    assert!(details.protocols.contains(&"sparkpost"));
    assert!(details.attachment_support);
  }

  #[tokio::test]
  async fn test_send_default_payload() {
    // Matches upstream (apprise-go golden "default")
    let server = mock(200).await;
    let svc = from_url("sparkpost://sender@domain.com/apikey/recipient@domain.com").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title", NotifyFormat::Html))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/api/v1/transmissions/");
    assert_eq!(reqs[0].headers.get("authorization").unwrap(), "apikey");
    assert_eq!(reqs[0].headers.get("accept").unwrap(), "application/json");
    assert_eq!(reqs[0].headers.get("content-type").unwrap(), "application/json");
    assert_eq!(
      bodies(&server).await[0],
      json!({
        "options": { "open_tracking": false, "click_tracking": false },
        "content": {
          "from": { "name": "Apprise Notifications", "email": "sender@domain.com" },
          "subject": "title",
          "reply_to": "sender@domain.com",
          "html": "body",
        },
        "substitution_data": {},
        "recipients": [{ "address": { "email": "recipient@domain.com" } }],
      })
    );
  }

  #[tokio::test]
  async fn test_send_batch_copies_headers_tokens() {
    // Verified against upstream with pycapture
    let server = mock(200).await;
    let svc = parse(
      "sparkpost://user@example.com/apikey/user1@example.com/Jo:jo@example.com?cc=Chris<cc@example.com>,cc2@example.com&bcc=bcc@example.com&+X-Header=val&:tok=1&name=Sender&batch=yes",
    );
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", NotifyFormat::Text))).await.unwrap());
    assert_eq!(
      bodies(&server).await,
      vec![json!({
        "options": { "open_tracking": false, "click_tracking": false },
        "content": {
          "from": { "name": "Sender", "email": "user@example.com" },
          "subject": ".",
          "reply_to": "Sender <user@example.com>",
          "text": "body",
          "headers": { "X-Header": "val", "CC": "cc@example.com,cc2@example.com" },
        },
        "substitution_data": { "tok": "1" },
        "recipients": [
          { "address": { "email": "user1@example.com" } },
          { "address": { "email": "jo@example.com", "name": "Jo" } },
          { "address": { "email": "cc@example.com", "header_to": "user1@example.com", "name": "Chris" } },
          { "address": { "email": "cc2@example.com", "header_to": "user1@example.com" } },
          { "address": { "email": "bcc@example.com", "header_to": "user1@example.com" } },
        ],
      })]
    );
  }

  #[tokio::test]
  async fn test_send_eu_region_and_attachments() {
    let server = mock(200).await;
    let svc = from_url("sparkpost://sender@example.com/key/user@example.com?region=eu").unwrap();
    let mut ctx = ctx("title", NotifyFormat::Html);
    ctx.attachments = vec![Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() }];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(parse("sparkpost://sender@example.com/key?region=eu").api_base(), "https://api.eu.sparkpost.com");
    assert_eq!(parse("sparkpost://sender@example.com/key").api_base(), "https://api.sparkpost.com");
    assert_eq!(bodies(&server).await[0]["content"]["attachments"], json!([{ "name": "report.pdf", "type": "application/pdf", "data": "UERGREFUQQ==" }]));
  }

  #[tokio::test]
  async fn test_send_retries_rate_limits() {
    // Upstream test_plugin_sparkpost_throttling: two 429s then a 200 succeeds
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(429)).up_to_n_times(2).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let svc = parse("sparkpost://user@localhost.localdomain/abc123");
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap());
    let bodies = bodies(&server).await;
    assert_eq!(bodies.len(), 3);
    // Every retry carries the recipients again
    assert!(bodies.iter().all(|b| b["recipients"] == json!([{ "address": { "email": "user@localhost.localdomain" } }])));

    // Three 429s in a row fail
    let server = mock(429).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap());
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 403] {
      let server = mock(status).await;
      let svc = parse(&format!("sparkpost://user@localhost.localdomain/{}", "n".repeat(32)));
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap(), "status {}", status);
      // Only a 429 is retried
      assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    let server = mock(200).await;
    let svc = parse(&format!("sparkpost://user@localhost.localdomain/{}/invalid", "i".repeat(32)));
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", NotifyFormat::Html))).await.unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());
  }
}
