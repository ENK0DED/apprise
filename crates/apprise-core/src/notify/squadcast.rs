//! Squadcast (now SolarWinds Incident Response), through an "Incident
//! Webhook" alert source:
//!
//!   squadcast://{webhook_key}
//!
//! The key ends the webhook URL Squadcast hands out
//! (`https://api.squadcast.com/v2/incidents/api/{webhook_key}`), which is
//! accepted as well. Accounts hosted in Europe add `?region=eu`. See
//! <https://support.squadcast.com/integrations/incident-webhook-incident-webhook-api>.

use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::{NotifyFormat, NotifyType};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `SQUADCAST_ACTIONS`, in the order a partial `?action=` is matched.
const ACTIONS: &[&str] = &["map", "trigger", "resolve"];

/// The webhook host of each region (upstream `SQUADCAST_API_LOOKUP`).
const REGIONS: &[(&str, &str)] = &[("us", "https://api.squadcast.com"), ("eu", "https://api.eu.squadcast.com")];

/// Upstream `SQUADCAST_PRIORITIES`.
const PRIORITIES: &[&str] = &["P1", "P2", "P3", "P4", "P5"];

/// Upstream `SQUADCAST_HTTP_ERROR_MAP`.
const HTTP_ERRORS: &[(u16, &str)] = &[
  (401, "Unauthorized - Invalid Webhook Key."),
  (404, "The webhook key could not be found."),
  (413, "The payload is too large."),
  (429, "Too many requests were made."),
];

/// Upstream's default `title_maxlen`, which caps a message taken from the body.
const MESSAGE_MAXLEN: usize = 250;

pub struct Squadcast {
  key: String,
  region: &'static str,
  action: &'static str,
  dedup: Option<String>,
  priority: Option<&'static str>,
  incident_tags: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Squadcast {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let key = arg(url, "key").or_else(|| url.host.clone()).unwrap_or_default();
    if !incident::is_key(&key) {
      tracing::warn!("An invalid Squadcast webhook key ({}) was specified.", key);
      return None;
    }

    let region = match arg(url, "region") {
      Some(r) => match REGIONS.iter().find(|(name, _)| *name == r.trim().to_lowercase()) {
        Some((name, _)) => *name,
        None => {
          tracing::warn!("The Squadcast region specified ({}) is invalid.", r);
          return None;
        }
      },
      None => "us",
    };

    let action = match arg(url, "action") {
      Some(a) => match incident::match_choice(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The Squadcast action specified ({}) is invalid.", a);
          return None;
        }
      },
      None => "map",
    };

    // A plain number such as 1 is read as P1
    let priority = match arg(url, "priority").map(|p| p.trim().to_uppercase()).filter(|p| !p.is_empty()) {
      Some(p) => {
        let p = if p.chars().all(|c| c.is_ascii_digit()) { format!("P{}", p) } else { p };
        match PRIORITIES.iter().copied().find(|v| *v == p) {
          Some(p) => Some(p),
          None => {
            tracing::warn!("The Squadcast priority specified ({}) is invalid.", p);
            return None;
          }
        }
      }
      None => None,
    };

    Some(Self {
      key,
      region,
      action,
      // A fixed event ID lets several notifications share one incident
      dedup: arg(url, "dedup").map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
      priority,
      incident_tags: incident::unquoted_map(&url.qsd_add),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Squadcast",
      service_url: Some("https://www.squadcast.com/"),
      setup_url: Some("https://appriseit.com/services/squadcast/"),
      protocols: vec!["squadcast"],
      description: "Trigger and resolve Squadcast incidents through an Incident Webhook.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Work out whether we trigger or resolve the incident
    let resolve = self.action == "resolve" || (self.action == "map" && ctx.notify_type == NotifyType::Success);

    // Squadcast resolves every incident without an event ID when it
    // receives another one, so one is always sent. Our own key wins; a
    // trigger gets its own each time (kept across retries) and map and
    // resolve take a key that stays the same, from the title.
    let event_id = match &self.dedup {
      Some(key) => key.clone(),
      None if self.action == "trigger" => incident::retry_key("event_id"),
      None => incident::title_key(&ctx.title, &ctx.body),
    };

    let mut payload = json!({
      // The message is the plain text incident title
      "message": incident::plain_title(&ctx.title, &ctx.body, MESSAGE_MAXLEN),
      "description": ctx.body,
      "status": if resolve { "resolve" } else { "trigger" },
      "event_id": event_id,
    });
    if let Some(priority) = self.priority {
      payload["priority"] = json!(priority);
    }
    if !self.incident_tags.is_empty() {
      payload["tags"] = incident::json_object(&self.incident_tags);
    }
    payload
  }
}

#[async_trait]
impl Notify for Squadcast {
  fn schemas(&self) -> &[&str] {
    &["squadcast"]
  }
  fn service_name(&self) -> &str {
    "Squadcast"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The incident description supports markdown
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  // Squadcast refuses payloads over 30KB; this leaves room for the rest
  fn body_maxlen(&self) -> usize {
    30000
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["squadcast".into(), self.region.into(), self.key.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = REGIONS.iter().find(|(name, _)| *name == self.region).map(|(_, base)| *base).unwrap_or(REGIONS[0].1);
    let client = build_client(self.verify_certificate)?;
    let request = client.post(format!("{}/v2/incidents/api/{}", service_base(base), self.key)).header("User-Agent", APP_ID).json(&self.payload(ctx));
    incident::exchange(request, "Squadcast incident", &[200, 201, 202, 204], HTTP_ERRORS).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const KEY: &str = "7f550a9f4c44173a37664d938f1355f0f92a47a7";

  fn squadcast(url: &str) -> Squadcast {
    Squadcast::from_url(&crate::notify::registry::parse_service_url(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("squadcast://{KEY}"),
      format!("squadcast://?key={KEY}"),
      format!("squadcast://{KEY}?region=eu&action=resolve&dedup=disk-full&priority=p2&+severity=high"),
      format!("https://api.squadcast.com/v2/incidents/api/{KEY}"),
      format!("https://api.eu.squadcast.com/v2/incidents/api/{KEY}?priority=1"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "squadcast://".to_string(),
      "squadcast://invalid!key".to_string(),
      format!("squadcast://{KEY}?region=invalid"),
      format!("squadcast://{KEY}?action=invalid"),
      format!("squadcast://{KEY}?priority=P9"),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = squadcast(&format!("squadcast://{KEY}?action=res&dedup=%20key%20&priority=3&+severity=high&+team=ops"));
    assert_eq!(obj.key, KEY);
    assert_eq!(obj.region, "us");
    assert_eq!(obj.action, "resolve");
    assert_eq!(obj.dedup.as_deref(), Some("key"));
    assert_eq!(obj.priority, Some("P3"));
    assert_eq!(obj.incident_tags.len(), 2);
    assert_eq!(squadcast(&format!("squadcast://{KEY}?priority=p5&region=EU")).priority, Some("P5"));
    assert_eq!(squadcast(&format!("squadcast://{KEY}?region=EU")).region, "eu");
    let obj = squadcast(&format!("squadcast://{KEY}"));
    assert_eq!(obj.action, "map");
    assert!(obj.priority.is_none() && obj.dedup.is_none());
    // The EU webhook URL keeps its region
    let obj = squadcast(&format!("https://api.eu.squadcast.com/v2/incidents/api/{KEY}"));
    assert_eq!((obj.region, obj.key.as_str()), ("eu", KEY));
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v2/incidents/api/{KEY}")))
      .and(header("content-type", "application/json"))
      .and(body_json(json!({
        "message": "Disk & <db01>",
        "description": "Usage at **95%** on `_db01_`.",
        "status": "resolve",
        "event_id": "disk-full",
        "priority": "P3",
        "tags": {"severity": "high", "team": "ops"},
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("squadcast://{KEY}?action=res&dedup=disk-full&priority=3&+severity=high&+team=ops")).unwrap();
    let ctx = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("Disk & <db01>", "Usage at **95%** on `_db01_`.", NotifyType::Info) };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_text_body() {
    // Matches pycapture.py with --input-format text
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v2/incidents/api/{KEY}")))
      .and(body_json(json!({
        "message": "Server_1 is *down*",
        "description": "Server\\_1 is \\*down\\*",
        "status": "trigger",
        "event_id": "cbfa97dfe3adf377e475f4bd8a4f1d8d34d440bcf58a3d013ebe3461b9683304",
        "priority": "P3",
        "tags": {"severity": "high"},
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("squadcast://{KEY}?priority=3&+severity=high")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server_1 is *down*", NotifyType::Info))).await.is_success());
  }

  #[tokio::test]
  async fn test_send_eu_native_url() {
    // Matches pycapture.py for the EU webhook URL with --type success
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/incidents/api/abc"))
      .and(body_json(json!({
        "message": "T",
        "description": "B",
        "status": "resolve",
        "event_id": "e632b7095b0bf32c260fa4c539e9fd7b852d0de454e9be26f24d0d6f91d069d3",
        "priority": "P1",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("https://api.eu.squadcast.com/v2/incidents/api/abc?priority=1").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Success))).await.unwrap());
  }

  #[test]
  fn test_actions() {
    let sent = |obj: &Squadcast, title: &str, body: &str, notify_type| obj.payload(&ctx(title, body, notify_type));

    // The default action maps the notification type
    let obj = squadcast(&format!("squadcast://{KEY}"));
    for (notify_type, status) in
      [(NotifyType::Info, "trigger"), (NotifyType::Warning, "trigger"), (NotifyType::Failure, "trigger"), (NotifyType::Success, "resolve")]
    {
      assert_eq!(sent(&obj, "Disk full", "db01", notify_type)["status"], status);
    }
    // The same title always produces the same event ID
    let raised = sent(&obj, "Disk full", "a", NotifyType::Failure)["event_id"].clone();
    assert_eq!(sent(&obj, "Disk full", "b", NotifyType::Success)["event_id"], raised);
    assert_ne!(sent(&obj, "CPU hot", "a", NotifyType::Info)["event_id"], raised);
    // Without a title, the first line of the message is used
    assert_eq!(sent(&obj, "", "Disk full\nat 95%", NotifyType::Info)["event_id"], raised);
    assert_eq!(sent(&obj, "", &"x".repeat(300), NotifyType::Info)["message"], "x".repeat(250));

    // Trigger triggers a new incident every time, even with the same title
    let obj = squadcast(&format!("squadcast://{KEY}?action=trigger"));
    let first = sent(&obj, "Disk full", "ok", NotifyType::Success);
    assert_eq!(first["status"], "trigger");
    assert_ne!(sent(&obj, "Disk full", "ok", NotifyType::Info)["event_id"], first["event_id"]);

    let obj = squadcast(&format!("squadcast://{KEY}?action=RESOLVE"));
    assert_eq!(sent(&obj, "", "bad", NotifyType::Failure)["status"], "resolve");
  }

  #[tokio::test]
  async fn test_retry_keeps_event_id() {
    let server = MockServer::start().await;
    let route = format!("/v2/incidents/api/{KEY}");
    Mock::given(method("POST")).and(path(route.clone())).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path(route.clone())).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("squadcast://{KEY}?action=trigger&retry=1&wait=0")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server down", NotifyType::Info))).await.is_success());
    let requests = server.received_requests().await.unwrap();
    let ids: Vec<Value> =
      requests.iter().filter(|r| r.url.path() == route).map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["event_id"].clone()).collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(format!("/v2/incidents/api/{KEY}"))).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("squadcast://{KEY}")).unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b", NotifyType::Info))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }

  #[test]
  fn test_service_details() {
    let svc = from_url(&format!("squadcast://{KEY}")).unwrap();
    assert_eq!(svc.service_name(), "Squadcast");
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!(svc.body_maxlen(), 30000);
    assert_eq!(Squadcast::static_details().protocols, vec!["squadcast"]);
  }
}
