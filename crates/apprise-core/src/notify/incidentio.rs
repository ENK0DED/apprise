//! incident.io, through an "HTTP" alert source:
//!
//!   incidentio://{source_id}/{token}
//!
//! The source ID ends the URL incident.io shows for the source
//! (`https://api.incident.io/v2/alert_events/http/{source_id}`), which is
//! accepted as well with its `?token=`. See
//! <https://docs.incident.io/alerts/http>.

use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::{NotifyFormat, NotifyType};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd::validate_token;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `INCIDENTIO_ACTIONS`, in the order a partial `?action=` is matched.
const ACTIONS: &[&str] = &["map", "trigger", "resolve"];

/// Upstream `INCIDENTIO_HTTP_ERROR_MAP`.
const HTTP_ERRORS: &[(u16, &str)] = &[
  (401, "Unauthorized - Invalid Token."),
  (404, "The alert source could not be found."),
  (413, "The payload is too large."),
  (422, "The alert was rejected as invalid."),
  (429, "Too many requests were made."),
];

/// Upstream's default `title_maxlen`, which caps a title taken from the body.
const TITLE_MAXLEN: usize = 250;

pub struct IncidentIO {
  source: String,
  token: String,
  action: &'static str,
  dedup: Option<String>,
  click: Option<String>,
  metadata: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl IncidentIO {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The alert source ID is the host and the token the first path entry
    let source = arg(url, "source").or_else(|| url.host.clone()).unwrap_or_default();
    if source.is_empty() || !source.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid incident.io alert source ID ({}) was specified.", source);
      return None;
    }
    let Some(token) = arg(url, "token").or_else(|| url.path_parts.first().cloned()).and_then(|t| validate_token(&t)) else {
      tracing::warn!("An invalid incident.io token was specified.");
      return None;
    };

    let action = match arg(url, "action") {
      Some(a) => match incident::match_choice(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The incident.io action specified ({}) is invalid.", a);
          return None;
        }
      },
      None => "map",
    };

    let trimmed = |key: &str| arg(url, key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    Some(Self {
      source,
      token,
      action,
      // A fixed key lets several notifications update the same alert
      dedup: trimmed("dedup"),
      // A link back to wherever the alert came from
      click: trimmed("click"),
      metadata: incident::unquoted_map(&url.qsd_add),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "incident.io",
      service_url: Some("https://incident.io/"),
      setup_url: Some("https://appriseit.com/services/incidentio/"),
      protocols: vec!["incidentio"],
      description: "Raise and resolve incident.io alerts through an HTTP alert source.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Work out whether we raise or resolve the alert
    let resolve = self.action == "resolve" || (self.action == "map" && ctx.notify_type == NotifyType::Success);

    // Our own key always wins. A trigger raises its own alert each time
    // (the key is kept across retries); map and resolve need a key that
    // stays the same between notifications, taken from the title.
    let dedup = match &self.dedup {
      Some(key) => key.clone(),
      None if self.action == "trigger" => incident::retry_key("dedup"),
      None => incident::title_key(&ctx.title, &ctx.body),
    };

    // A title is required and is plain text. A body with no input format
    // has no escapes Apprise added, so it is used as written.
    let title = match ctx.body_passthrough {
      true if ctx.title.is_empty() => ctx.body.chars().take(TITLE_MAXLEN).collect(),
      _ => incident::plain_title(&ctx.title, &ctx.body, TITLE_MAXLEN),
    };
    let mut payload = json!({
      "title": title,
      "description": ctx.body,
      "status": if resolve { "resolved" } else { "firing" },
      "deduplication_key": dedup,
    });
    if let Some(click) = &self.click {
      payload["source_url"] = json!(click);
    }
    if !self.metadata.is_empty() {
      payload["metadata"] = incident::json_object(&self.metadata);
    }
    payload
  }
}

#[async_trait]
impl Notify for IncidentIO {
  fn schemas(&self) -> &[&str] {
    &["incidentio"]
  }
  fn service_name(&self) -> &str {
    "incident.io"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The alert description supports markdown
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["incidentio".into(), self.source.clone(), self.token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let request = client
      .post(format!("{}/v2/alert_events/http/{}", service_base("https://api.incident.io"), self.source))
      .header("User-Agent", APP_ID)
      .header("Authorization", format!("Bearer {}", self.token))
      .json(&self.payload(ctx));
    incident::exchange(request, "incident.io alert", &[200, 201, 202], HTTP_ERRORS).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const SOURCE: &str = "01GW2G3V0S59R238FAHPDS1R66";

  fn incidentio(url: &str) -> IncidentIO {
    IncidentIO::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("incidentio://{SOURCE}/secret"),
      format!("incidentio://?source={SOURCE}&token=secret"),
      format!("incidentio://{SOURCE}/secret?action=resolve&dedup=disk-full&click=https://example.com/alert&+service=web"),
      // The URL incident.io shows when the source is created
      format!("https://api.incident.io/v2/alert_events/http/{SOURCE}?token=secret"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "incidentio://".to_string(),
      format!("incidentio://{SOURCE}"),
      "incidentio://invalid-source!/token".to_string(),
      format!("incidentio://{SOURCE}/secret?action=invalid"),
      format!("https://api.incident.io/v2/alert_events/http/{SOURCE}"),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = incidentio(&format!("incidentio://{SOURCE}/secret?action=res&dedup=%20key%20&click=%20%20&+service=web&+team=ops"));
    assert_eq!((obj.source.as_str(), obj.token.as_str()), (SOURCE, "secret"));
    assert_eq!(obj.action, "resolve");
    assert_eq!(obj.dedup.as_deref(), Some("key"));
    assert!(obj.click.is_none());
    assert_eq!(obj.metadata.len(), 2);
    let obj = incidentio(&format!("incidentio://{SOURCE}/secret"));
    assert_eq!(obj.action, "map");
    // ?source= and ?token= win
    let obj = incidentio("incidentio://01ABC/one?source=01DEF&token=two");
    assert_eq!((obj.source.as_str(), obj.token.as_str()), ("01DEF", "two"));
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v2/alert_events/http/{SOURCE}")))
      .and(header("authorization", "Bearer secret"))
      .and(header("content-type", "application/json"))
      .and(body_json(json!({
        "title": "Disk & <db01>",
        "description": "Usage at **95%** on `_db01_`.",
        "status": "resolved",
        "deduplication_key": "disk-full",
        "source_url": "https://example.com/alert",
        "metadata": {"service": "web", "team": "ops"},
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("incidentio://{SOURCE}/secret?action=res&dedup=disk-full&click=https://example.com/alert&+service=web&+team=ops")).unwrap();
    let ctx = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("Disk & <db01>", "Usage at **95%** on `_db01_`.", NotifyType::Info) };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_text_body() {
    // Matches pycapture.py with --input-format text and --type success
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v2/alert_events/http/{SOURCE}")))
      .and(body_json(json!({
        "title": "Server_1 is *down*",
        "description": "Server\\_1 is \\*down\\*",
        "status": "resolved",
        "deduplication_key": "cbfa97dfe3adf377e475f4bd8a4f1d8d34d440bcf58a3d013ebe3461b9683304",
        "source_url": "https://x",
        "metadata": {"service": "web"},
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("incidentio://{SOURCE}/secret?click=https://x&+service=web")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server_1 is *down*", NotifyType::Success))).await.is_success());
  }

  #[test]
  fn test_title_from_a_passthrough_body_keeps_its_backslashes() {
    // Matches pycapture.py without --input-format, then with markdown
    let obj = incidentio(&format!("incidentio://{SOURCE}/secret?dedup=k"));
    let body = r"Usage at **95%** on \_db01\_ <i>";
    let passthrough = NotifyContext { body_passthrough: true, ..ctx("", body, NotifyType::Info) };
    let payload = obj.payload(&passthrough);
    assert_eq!((payload["title"].as_str(), payload["description"].as_str()), (Some(body), Some(body)));
    let markdown = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("", body, NotifyType::Info) };
    assert_eq!(obj.payload(&markdown)["title"], r"Usage at **95%** on _db01_ <i>");
    // A passthrough title is still cut to the title limit
    let long = NotifyContext { body: "x".repeat(300), ..passthrough };
    assert_eq!(obj.payload(&long)["title"].as_str().map(str::len), Some(TITLE_MAXLEN));
  }

  #[tokio::test]
  async fn test_send_native_url() {
    // Matches pycapture.py for the native URL
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v2/alert_events/http/{SOURCE}")))
      .and(header("authorization", "Bearer secret"))
      .and(body_json(json!({
        "title": "T",
        "description": "B",
        "status": "firing",
        "deduplication_key": "e632b7095b0bf32c260fa4c539e9fd7b852d0de454e9be26f24d0d6f91d069d3",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("https://api.incident.io/v2/alert_events/http/{SOURCE}?token=secret")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Info))).await.unwrap());
  }

  #[test]
  fn test_actions() {
    let sent = |obj: &IncidentIO, title: &str, body: &str, notify_type| obj.payload(&ctx(title, body, notify_type));

    // The default action maps the notification type
    let obj = incidentio(&format!("incidentio://{SOURCE}/secret"));
    for (notify_type, status) in
      [(NotifyType::Info, "firing"), (NotifyType::Warning, "firing"), (NotifyType::Failure, "firing"), (NotifyType::Success, "resolved")]
    {
      assert_eq!(sent(&obj, "Disk full", "db01", notify_type)["status"], status);
    }
    // The same title always produces the same key
    let raised = sent(&obj, "Disk full", "a", NotifyType::Failure)["deduplication_key"].clone();
    assert_eq!(sent(&obj, "Disk full", "b", NotifyType::Success)["deduplication_key"], raised);
    assert_ne!(sent(&obj, "CPU hot", "a", NotifyType::Info)["deduplication_key"], raised);
    // Without a title, the first line of the message is used
    assert_eq!(sent(&obj, "", "Disk full\nat 95%", NotifyType::Info)["deduplication_key"], raised);
    assert_ne!(sent(&obj, "", "one", NotifyType::Info)["deduplication_key"], sent(&obj, "", "two", NotifyType::Info)["deduplication_key"]);
    // A long untitled body is capped as the title
    assert_eq!(sent(&obj, "", &"x".repeat(300), NotifyType::Info)["title"], "x".repeat(250));

    // Trigger raises a new alert every time, even with the same title
    let obj = incidentio(&format!("incidentio://{SOURCE}/secret?action=trigger"));
    let first = sent(&obj, "Disk full", "ok", NotifyType::Success);
    assert_eq!(first["status"], "firing");
    assert_ne!(sent(&obj, "Disk full", "ok", NotifyType::Info)["deduplication_key"], first["deduplication_key"]);

    let obj = incidentio(&format!("incidentio://{SOURCE}/secret?action=RESOLVE"));
    assert_eq!(sent(&obj, "", "bad", NotifyType::Failure)["status"], "resolved");
  }

  #[tokio::test]
  async fn test_retry_keeps_key() {
    let server = MockServer::start().await;
    let route = format!("/v2/alert_events/http/{SOURCE}");
    Mock::given(method("POST")).and(path(route.clone())).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path(route.clone())).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("incidentio://{SOURCE}/secret?action=trigger&retry=1&wait=0")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server down", NotifyType::Info))).await.is_success());
    let requests = server.received_requests().await.unwrap();
    let keys: Vec<Value> =
      requests.iter().filter(|r| r.url.path() == route).map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["deduplication_key"].clone()).collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1]);
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path(format!("/v2/alert_events/http/{SOURCE}")))
        .respond_with(ResponseTemplate::new(status))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url(&format!("incidentio://{SOURCE}/secret")).unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b", NotifyType::Info))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }

  #[test]
  fn test_service_details() {
    let svc = from_url(&format!("incidentio://{SOURCE}/secret")).unwrap();
    assert_eq!(svc.service_name(), "incident.io");
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!(IncidentIO::static_details().protocols, vec!["incidentio"]);
  }
}
