use crate::error::NotifyError;
use crate::notify::sms_targets::{is_phone_no, parse_phone_no, unquoted_host, url_credentials};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use async_trait::async_trait;
use serde_json::{Map, Value, json};

/// Recipient payload keys a `:key=value` mapping may never override.
const RESERVED_KEYWORDS: &[&str] = &["mobiles"];

pub struct Msg91 {
  authkey: String,
  template: String,
  targets: Vec<String>,
  short_url: bool,
  /// `:key=value` URL arguments, in key order
  template_mapping: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Msg91 {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (url_user, _) = url_credentials(url);
    // msg91://template_id@authkey/phone1/phone2
    let authkey = unquoted_host(url).unwrap_or_default();
    if authkey.is_empty() || !authkey.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid MSG91 Authentication Key was specified.");
      return None;
    }
    // Upstream: `^[a-z0-9 _-]+$`, stored trimmed and never blank
    let template = url_user.clone().unwrap_or_default();
    if !template.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-')) || template.trim().is_empty() {
      tracing::warn!("An invalid MSG91 Template ID ({}) was specified.", template);
      return None;
    }
    let template = template.trim().to_string();

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|v| !v.trim().is_empty()) {
      entries.extend(parse_phone_no([to], false));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(&entries, false) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(phone.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    let short_url = url.get("short_url").filter(|v| !v.trim().is_empty()).map(parse_bool).unwrap_or(false);

    let mut template_mapping: Vec<(String, String)> =
      url.qsd.iter().filter_map(|(k, v)| k.strip_prefix(':').map(|k| (k.to_string(), v.trim().to_string()))).collect();
    template_mapping.sort();

    Some(Self { authkey, template, targets, short_url, template_mapping, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "MSG91",
      service_url: Some("https://msg91.com"),
      setup_url: Some("https://appriseit.com/services/msg91/"),
      protocols: vec!["msg91"],
      description: "Send SMS via MSG91.",
      attachment_support: false,
    }
  }

  /// One recipient entry (minus `mobiles`) with the template mapping applied:
  /// `:body=message` renames a field, an empty `:type=` removes it and any
  /// other key is added as is.
  fn recipient_payload(&self, ctx: &NotifyContext) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("body".into(), Value::String(ctx.body.clone()));
    payload.insert("type".into(), Value::String(ctx.notify_type.as_str().into()));
    for (key, value) in &self.template_mapping {
      if RESERVED_KEYWORDS.contains(&key.as_str()) {
        tracing::warn!("Ignoring MSG91 custom payload entry {}", key);
        continue;
      }
      if let Some(existing) = payload.remove(key) {
        if !value.is_empty() {
          payload.insert(value.clone(), existing);
        }
      } else {
        payload.insert(key.clone(), Value::String(value.clone()));
      }
    }
    payload
  }
}

#[async_trait]
impl Notify for Msg91 {
  fn schemas(&self) -> &[&str] {
    &["msg91"]
  }
  fn service_name(&self) -> &str {
    "MSG91"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    160
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no MSG91 targets to notify.");
      return Ok(false);
    }
    let template = self.recipient_payload(ctx);
    let recipients: Vec<Value> = self
      .targets
      .iter()
      .map(|target| {
        let mut recipient = template.clone();
        recipient.insert("mobiles".into(), Value::String(target.clone()));
        Value::Object(recipient)
      })
      .collect();
    let payload = json!({
      "template_id": self.template,
      "short_url": if self.short_url { 1 } else { 0 },
      "recipients": recipients,
    });

    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/api/v5/flow/", service_base("https://control.msg91.com"));
    let result = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("authkey", &self.authkey)
      .body(serde_json::to_string(&payload)?)
      .send()
      .await;
    match result {
      Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
        tracing::info!("Sent MSG91 notification to {}.", self.targets.join(","));
        Ok(true)
      }
      Ok(resp) => {
        let status = resp.status();
        tracing::warn!(
          "Failed to send MSG91 notification to {}: {}error={}.",
          self.targets.join(","),
          status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
          status.as_u16()
        );
        tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        Ok(false)
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending MSG91:{} notification.", self.targets.join(","));
        tracing::debug!("Socket Exception: {}", e);
        Ok(false)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn msg91(url: &str) -> Msg91 {
    Msg91::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "msg91://".to_string(),
      "msg91://-".to_string(),
      // Valid authkey but no template
      format!("msg91://{}", "a".repeat(23)),
      // Blank template, or one with invalid characters
      format!("msg91://%20%20@{}", "a".repeat(23)),
      format!("msg91://t*t@{}", "a".repeat(23)),
      // Authkey may only hold letters and digits
      format!("msg91://{}@a_b", "t".repeat(20)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let tmpl = "t".repeat(20);
    let key = "a".repeat(23);
    let urls = vec![
      // No targets (notify fails)
      format!("msg91://{}@{}", tmpl, key),
      // Invalid target dropped (notify fails)
      format!("msg91://{}@{}/abcd", tmpl, key),
      format!("msg91://{}@{}/15551232000", tmpl, key),
      format!("msg91://{}@{}/?to=15551232000&short_url=no", tmpl, key),
      format!("msg91://{}@{}/15551232000?short_url=yes", tmpl, key),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let tmpl = "t".repeat(20);
    let key = "a".repeat(23);
    let m = msg91(&format!("msg91://{}@{}/15551232000/abcd/+1%20(555)%20123-3456?short_url=yes&to=15551232001", tmpl, key));
    assert_eq!(m.authkey, key);
    assert_eq!(m.template, tmpl);
    assert_eq!(m.targets, vec!["15551232000", "15551233456", "15551232001"]);
    assert!(m.short_url);
    assert!(!msg91(&format!("msg91://{}@{}/15551232000", tmpl, key)).short_url);
  }

  #[test]
  fn test_service_details() {
    let details = Msg91::static_details();
    assert_eq!(details.service_name, "MSG91");
    assert!(details.protocols.contains(&"msg91"));
    assert!(!details.attachment_support);
  }

  #[tokio::test]
  async fn test_send_payload() {
    // Matches upstream pycapture / apprise-go golden output
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v5/flow/"))
      .and(header("authkey", "authkey123"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({
        "template_id": "template1",
        "short_url": 0,
        "recipients": [{"mobiles": "15551231234", "body": "title\r\nbody", "type": "info"}],
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("msg91://template1@authkey123/15551231234").unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_template_mapping() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({
        "template_id": "template1",
        "short_url": 1,
        "recipients": [{"mobiles": "15551231234", "message": "hello", "category": "warning", "foo": "bar"}],
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("msg91://template1@authkey123/15551231234?short_url=yes&:body=message&:type=category&:foo=bar&:mobiles=ignored").unwrap();
    let ctx = NotifyContext { body: "hello".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_template_mapping_removes_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({
        "template_id": "12345",
        "short_url": 0,
        "recipients": [{"mobiles": "15551233456", "cat": "info"}],
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("msg91://12345@{}/+1%20(555)%20123-3456?:body&:type=cat", "b".repeat(32))).unwrap();
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    // No valid targets
    let svc = from_url(&format!("msg91://{}@{}/abcd", "t".repeat(20), "a".repeat(23))).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    // Anything but 200 fails
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(202)).expect(1).mount(&server).await;
    let svc = from_url(&format!("msg91://{}@{}/15551232000", "t".repeat(20), "a".repeat(23))).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Connection errors fail without raising
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
