//! ilert, through the Events API of an "API" alert source:
//!
//!   ilert://{integration_key}
//!
//! See <https://docs.ilert.com/developer-docs/rest-api/api-reference/events>.

use crate::asset::NotifyImageSize;
use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::{NotifyFormat, NotifyType};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::qsd_bool;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `ILERT_ACTIONS`, in the order a partial `?action=` is matched.
const ACTIONS: &[&str] = &["map", "trigger", "acknowledge", "resolve"];

/// Upstream `ILERT_PRIORITIES`.
const PRIORITIES: &[&str] = &["high", "low"];

/// Upstream `ILERT_HTTP_ERROR_MAP`.
const HTTP_ERRORS: &[(u16, &str)] = &[(400, "Bad Request - Check the integration key and payload."), (429, "Too many requests were made.")];

/// Upstream's default `title_maxlen`, which caps a summary taken from the body.
const SUMMARY_MAXLEN: usize = 250;

pub struct ILert {
  key: String,
  action: &'static str,
  dedup: Option<String>,
  priority: Option<&'static str>,
  routing_key: Option<String>,
  click: Option<String>,
  include_image: bool,
  labels: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// An optional argument, trimmed, with blank values left out.
fn trimmed(url: &ParsedUrl, key: &str) -> Option<String> {
  arg(url, key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl ILert {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let key = arg(url, "key").or_else(|| url.host.clone()).unwrap_or_default();
    if !incident::is_key(&key) {
      tracing::warn!("An invalid ilert integration key ({}) was specified.", key);
      return None;
    }

    let action = match arg(url, "action") {
      Some(a) => match incident::match_choice(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The ilert action specified ({}) is invalid.", a);
          return None;
        }
      },
      None => "map",
    };

    let priority = match trimmed(url, "priority") {
      Some(p) => match incident::match_choice(PRIORITIES, &p) {
        Some(p) => Some(p),
        None => {
          tracing::warn!("The ilert priority specified ({}) is invalid.", p);
          return None;
        }
      },
      None => None,
    };

    Some(Self {
      key,
      action,
      // A fixed alert key lets several notifications share one alert
      dedup: trimmed(url, "dedup"),
      priority,
      // Send new alerts to a different escalation policy
      routing_key: trimmed(url, "route"),
      click: trimmed(url, "click"),
      include_image: qsd_bool(url, "image", true),
      labels: incident::unquoted_map(&url.qsd_add),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "ilert",
      service_url: Some("https://www.ilert.com/"),
      setup_url: Some("https://appriseit.com/services/ilert/"),
      protocols: vec!["ilert"],
      description: "Raise, accept and resolve ilert alerts through the Events API.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Map resolves on success and raises an alert otherwise
    let event_type = match self.action {
      "map" if ctx.notify_type == NotifyType::Success => "RESOLVE",
      "map" | "trigger" => "ALERT",
      "acknowledge" => "ACCEPT",
      _ => "RESOLVE",
    };

    // Our own key always wins. A trigger raises its own alert each time
    // (the key is kept across retries); every other action needs a key that
    // stays the same between notifications, taken from the title.
    let alert_key = match &self.dedup {
      Some(key) => key.clone(),
      None if self.action == "trigger" => incident::retry_key("alert_key"),
      None => incident::title_key(&ctx.title, &ctx.body),
    };

    // The summary is the plain text alert title. A body with no input
    // format has no escapes Apprise added, so it is used as written.
    let summary = match ctx.body_passthrough {
      true if ctx.title.is_empty() => ctx.body.chars().take(SUMMARY_MAXLEN).collect(),
      _ => incident::plain_title(&ctx.title, &ctx.body, SUMMARY_MAXLEN),
    };
    let mut payload = json!({
      "integrationKey": self.key,
      "eventType": event_type,
      "summary": summary,
      "details": ctx.body,
      "alertKey": alert_key,
    });
    if let Some(priority) = self.priority {
      payload["priority"] = json!(priority.to_uppercase());
    }
    if let Some(route) = &self.routing_key {
      payload["routingKey"] = json!(route);
    }
    if let Some(click) = &self.click {
      payload["links"] = json!([{ "href": click }]);
    }
    if self.include_image
      && let Some(image) = ctx.image_url(NotifyImageSize::Xy128)
    {
      payload["images"] = json!([{ "src": image, "alt": ctx.notify_type.as_str() }]);
    }
    if !self.labels.is_empty() {
      payload["labels"] = incident::json_object(&self.labels);
    }
    payload
  }
}

#[async_trait]
impl Notify for ILert {
  fn schemas(&self) -> &[&str] {
    &["ilert"]
  }
  fn service_name(&self) -> &str {
    "ilert"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The alert details support markdown
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["ilert".into(), self.key.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let request = client
      .post(format!("{}/api/events", service_base("https://api.ilert.com")))
      .header("User-Agent", APP_ID)
      .header("Accept", "application/json")
      .json(&self.payload(ctx));
    incident::exchange(request, "ilert event", &[200, 201, 202, 204], HTTP_ERRORS).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const KEY: &str = "il1api0123456789abcdef0123456789abcdef";
  const FAILURE_IMAGE: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-failure-128x128.png";

  fn ilert(url: &str) -> ILert {
    ILert::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("ilert://{KEY}"),
      format!("ilert://?key={KEY}"),
      format!("ilert://{KEY}?action=ack&dedup=disk-full&priority=high&route=ops&click=https://example.com&image=no&+env=prod"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = ["ilert://".to_string(), "ilert://invalid!key".to_string(), format!("ilert://{KEY}?action=invalid"), format!("ilert://{KEY}?priority=urgent")];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj =
      ilert(&format!("ilert://{KEY}?action=res&dedup=%20disk-full%20&priority=L&route=db-team&click=https://example.com/db01&image=no&+env=prod&+team=ops"));
    assert_eq!(obj.key, KEY);
    assert_eq!(obj.action, "resolve");
    assert_eq!(obj.dedup.as_deref(), Some("disk-full"));
    assert_eq!(obj.priority, Some("low"));
    assert_eq!(obj.routing_key.as_deref(), Some("db-team"));
    assert_eq!(obj.click.as_deref(), Some("https://example.com/db01"));
    assert!(!obj.include_image);
    assert_eq!(obj.labels, vec![("env".to_string(), "prod".to_string()), ("team".to_string(), "ops".to_string())]);

    let obj = ilert(&format!("ilert://{KEY}"));
    assert_eq!(obj.action, "map");
    assert!(obj.include_image);
    assert!(obj.dedup.is_none() && obj.priority.is_none() && obj.routing_key.is_none() && obj.click.is_none());
    assert_eq!(ilert(&format!("ilert://{KEY}?action=ACKNOWLEDGE")).action, "acknowledge");
  }

  #[tokio::test]
  async fn test_send_payload() {
    // A title and a markdown body
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/events"))
      .and(header("content-type", "application/json"))
      .and(header("accept", "application/json"))
      .and(body_json(json!({
        "integrationKey": KEY,
        "eventType": "RESOLVE",
        "summary": "Disk & <db01>",
        "details": "Usage at **95%** on `_db01_`.",
        "alertKey": "disk-full",
        "priority": "LOW",
        "routingKey": "db-team",
        "links": [{"href": "https://example.com/db01"}],
        "labels": {"env": "prod", "team": "ops"},
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc =
      from_url(&format!("ilert://{KEY}?action=res&dedup=disk-full&priority=L&route=db-team&click=https://example.com/db01&image=no&+env=prod&+team=ops"))
        .unwrap();
    let ctx = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("Disk & <db01>", "Usage at **95%** on `_db01_`.", NotifyType::Info) };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().filter(|r| r.url.path() == "/api/events").all(|r| !r.headers.contains_key("authorization")));
  }

  #[tokio::test]
  async fn test_send_text_body() {
    // Matches pycapture.py with --input-format text: without a title the
    // summary is the body without the escapes the conversion added
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/events"))
      .and(body_json(json!({
        "integrationKey": KEY,
        "eventType": "ALERT",
        "summary": "Server_1 is *down*",
        "details": "Server\\_1 is \\*down\\*",
        "alertKey": "cbfa97dfe3adf377e475f4bd8a4f1d8d34d440bcf58a3d013ebe3461b9683304",
        "priority": "HIGH",
        "routingKey": "ops",
        "links": [{"href": "https://example.com"}],
        "images": [{"src": FAILURE_IMAGE, "alt": "failure"}],
        "labels": {"env": "prod"},
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("ilert://{KEY}?priority=h&route=ops&click=https://example.com&+env=prod")));
    let result = crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server_1 is *down*", NotifyType::Failure))).await;
    assert!(result.is_success());
  }

  #[tokio::test]
  async fn test_summary_from_a_passthrough_body_keeps_its_backslashes() {
    // Matches upstream's notify(body_format=None), then body_format="markdown"
    let body = r"Usage at **95%** on \_db01\_ <i> & more";
    let info_image = FAILURE_IMAGE.replace("failure", "info");
    for (passthrough, format, summary) in [(true, NotifyFormat::Text, body), (false, NotifyFormat::Markdown, r"Usage at **95%** on _db01_ <i> & more")] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/api/events"))
        .and(body_json(json!({
          "integrationKey": KEY,
          "eventType": "ALERT",
          "summary": summary,
          "details": body,
          "alertKey": "k",
          "images": [{"src": info_image, "alt": "info"}],
        })))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
      let mut a = crate::Apprise::new();
      assert!(a.add(&format!("ilert://{KEY}?dedup=k")));
      let c = NotifyContext { body_passthrough: passthrough, body_format: format, ..ctx("", body, NotifyType::Info) };
      assert!(crate::notify::with_test_base(server.uri(), a.notify(&c)).await.is_success(), "passthrough={passthrough}");
    }
  }

  #[test]
  fn test_actions() {
    let sent = |obj: &ILert, title: &str, body: &str, notify_type| obj.payload(&ctx(title, body, notify_type));

    // The default action maps the notification type
    let obj = ilert(&format!("ilert://{KEY}"));
    for (notify_type, event_type) in
      [(NotifyType::Info, "ALERT"), (NotifyType::Warning, "ALERT"), (NotifyType::Failure, "ALERT"), (NotifyType::Success, "RESOLVE")]
    {
      assert_eq!(sent(&obj, "Disk full", "db01", notify_type)["eventType"], event_type);
    }
    // The same title always produces the same alert key
    let raised = sent(&obj, "Disk full", "a", NotifyType::Failure)["alertKey"].clone();
    assert_eq!(sent(&obj, "Disk full", "b", NotifyType::Success)["alertKey"], raised);
    assert_ne!(sent(&obj, "CPU hot", "a", NotifyType::Info)["alertKey"], raised);
    // Without a title, the first line of the message is used
    assert_eq!(sent(&obj, "", "Disk full\nat 95%", NotifyType::Info)["alertKey"], raised);

    // Trigger raises a new alert every time, even with the same title
    let obj = ilert(&format!("ilert://{KEY}?action=trigger"));
    let first = sent(&obj, "Disk full", "ok", NotifyType::Success);
    assert_eq!(first["eventType"], "ALERT");
    assert_ne!(sent(&obj, "Disk full", "ok", NotifyType::Info)["alertKey"], first["alertKey"]);

    // Acknowledge accepts the alert the title points to
    let p = sent(&ilert(&format!("ilert://{KEY}?action=ACKNOWLEDGE")), "Disk full", "on it", NotifyType::Info);
    assert_eq!(p["eventType"], "ACCEPT");
    assert_eq!(p["alertKey"], raised);

    // Resolve ignores the notification type
    assert_eq!(sent(&ilert(&format!("ilert://{KEY}?action=r")), "", "bad", NotifyType::Failure)["eventType"], "RESOLVE");

    // A long untitled body is capped as the summary
    assert_eq!(sent(&ilert(&format!("ilert://{KEY}")), "", &"x".repeat(300), NotifyType::Info)["summary"], "x".repeat(250));
  }

  #[tokio::test]
  async fn test_retry_keeps_key() {
    // The retry reuses the trigger's alert key so ilert sees one alert
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/events")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/events")).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("ilert://{KEY}?action=trigger&retry=1&wait=0")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server down", NotifyType::Info))).await.is_success());
    let requests = server.received_requests().await.unwrap();
    let keys: Vec<Value> =
      requests.iter().filter(|r| r.url.path() == "/api/events").map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["alertKey"].clone()).collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1]);
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [400, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/api/events")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("ilert://{KEY}")).unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b", NotifyType::Info))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }

  #[test]
  fn test_service_details() {
    let svc = from_url(&format!("ilert://{KEY}")).unwrap();
    assert_eq!(svc.service_name(), "ilert");
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!(svc.title_maxlen(), 250);
    assert_eq!(ILert::static_details().protocols, vec!["ilert"]);
  }
}
