use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

/// The channel notified when none is given.
const DEFAULT_CHANNEL: &str = "general";

const TITLE_MAXLEN: usize = 255;
const BODY_MAXLEN: usize = 1000;

/// Trigv push notifications through a workspace ingest API key.
pub struct Trigv {
  api_key: String,
  channels: Vec<String>,
  /// Self-hosted ingest gateway; `None` uses api.trigv.com
  host: Option<String>,
  port: Option<u16>,
  secure: bool,
  supplemental_url: Option<String>,
  image_url: Option<String>,
  event_type: Option<String>,
  /// Pushover-style priority; 1 or higher implies a time-sensitive delivery
  priority: Option<i64>,
  /// `standard` or `time_sensitive`
  urgency: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Trigv {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The API key is the user of a user@host URL, or the host itself
    let (api_key, host) = match &url.user {
      Some(user) => (unquote(user), url.host.clone()),
      None => (url.host.as_deref().map(unquote).unwrap_or_default(), None),
    };
    let api_key = api_key.trim().to_string();
    if !is_api_key(&api_key) {
      tracing::warn!("An invalid Trigv API Key was specified.");
      return None;
    }

    let mut entries = split_path(&url.path);
    if let Some(to) = url.get("to").filter(|v| !v.is_empty()) {
      entries.push(to.to_string());
    }
    let mut channels = Vec::new();
    for channel in parse_list(&entries) {
      // A single bad entry fails loudly rather than being dropped
      if !is_channel(&channel) {
        tracing::warn!("An invalid Trigv channel slug ({}) was specified.", channel);
        return None;
      }
      channels.push(channel);
    }
    if channels.is_empty() {
      channels.push(DEFAULT_CHANNEL.to_string());
    }

    let urgency = url.get("urgency").filter(|v| !v.is_empty()).map(|v| v.to_lowercase()).unwrap_or_else(|| "standard".to_string());
    if urgency != "standard" && urgency != "time_sensitive" {
      tracing::warn!("An invalid Trigv urgency ({}) was specified.", urgency);
      return None;
    }

    let opt = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(str::to_string);
    Some(Self {
      api_key,
      channels,
      // A port only applies alongside a custom host
      port: host.as_ref().and(url.port),
      host,
      secure: url.schema == "trigvs",
      supplemental_url: opt("url"),
      image_url: opt("image_url"),
      event_type: opt("event_type"),
      // Ignore a priority that is not an integer rather than reject the URL
      priority: url.get("priority").filter(|v| !v.is_empty()).and_then(|v| v.trim().parse().ok()),
      urgency,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Trigv",
      service_url: Some("https://trigv.com/"),
      setup_url: Some("https://trigv.com/docs/learn/api-keys"),
      protocols: vec!["trigv", "trigvs"],
      description: "Send push notifications to Trigv channels.",
      attachment_support: false,
    }
  }

  fn notify_url(&self) -> String {
    match &self.host {
      Some(host) => {
        let schema = if self.secure { "https" } else { "http" };
        let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        format!("{}://{}{}/api/v1/events", schema, host, port)
      }
      None => format!("{}/api/v1/events", service_base("https://api.trigv.com")),
    }
  }

  /// An explicit urgency wins, then a priority of 1 or more, then failures
  /// escalate to time-sensitive delivery.
  fn resolve_urgency(&self, notify_type: &NotifyType) -> &str {
    if self.urgency != "standard" {
      return &self.urgency;
    }
    if self.priority.is_some_and(|p| p >= 1) || *notify_type == NotifyType::Failure {
      return "time_sensitive";
    }
    "standard"
  }
}

/// `trgv_` + 8 alphanumerics + `_` + 32 alphanumerics (either case).
fn is_api_key(s: &str) -> bool {
  let lower = s.to_lowercase();
  let Some(rest) = lower.strip_prefix("trgv_") else { return false };
  let b = rest.as_bytes();
  b.len() == 41 && b[8] == b'_' && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_alphanumeric())
}

/// A workspace channel slug: alphanumeric first, then up to 119 of
/// alphanumerics, `_` and `-`.
fn is_channel(s: &str) -> bool {
  let b = s.as_bytes();
  !b.is_empty() && b.len() <= 120 && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Upstream `URLBase.split_path`: split on whitespace, commas and slashes,
/// then unquote each entry.
fn split_path(path: &str) -> Vec<String> {
  path.split([' ', '\t', '\r', '\n', ',', '\\', '/']).filter(|s| !s.is_empty()).map(unquote).collect()
}

/// Upstream `parse_list`: split on whitespace, commas, semicolons and
/// brackets, then sort and drop duplicates.
fn parse_list(items: &[String]) -> Vec<String> {
  let mut out: Vec<String> = items
    .iter()
    .flat_map(|s| s.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')).filter(|s| !s.is_empty()).map(str::to_string).collect::<Vec<_>>())
    .collect();
  out.sort();
  out.dedup();
  out
}

fn truncate_chars(s: &str, max: usize) -> String {
  s.chars().take(max).collect()
}

#[async_trait]
impl Notify for Trigv {
  fn schemas(&self) -> &[&str] {
    &["trigv", "trigvs"]
  }
  fn service_name(&self) -> &str {
    "Trigv"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    BODY_MAXLEN
  }
  fn title_maxlen(&self) -> usize {
    TITLE_MAXLEN
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // A title is required; the application description stands in
    let title = if !ctx.title.is_empty() {
      ctx.title.clone()
    } else if !ctx.asset.app_desc.is_empty() {
      ctx.asset.app_desc.clone()
    } else {
      "Apprise Alert".to_string()
    };
    let body = if ctx.body.is_empty() && ctx.title.is_empty() { title.clone() } else { ctx.body.clone() };
    let level = match ctx.notify_type {
      NotifyType::Info => "info",
      NotifyType::Success => "success",
      NotifyType::Warning => "warning",
      NotifyType::Failure => "error",
    };
    let urgency = self.resolve_urgency(&ctx.notify_type);
    let url = self.notify_url();
    let client = build_client(self.verify_certificate)?;

    let mut has_error = false;
    for channel in &self.channels {
      let mut payload = json!({
        "channel": channel,
        "title": truncate_chars(&title, TITLE_MAXLEN),
        "level": level,
      });
      if !body.is_empty() {
        payload["description"] = json!(truncate_chars(&body, BODY_MAXLEN));
      }
      if let Some(v) = &self.supplemental_url {
        payload["url"] = json!(v);
      }
      if let Some(v) = &self.image_url {
        payload["image_url"] = json!(v);
      }
      if let Some(v) = &self.event_type {
        payload["event_type"] = json!(v);
      }
      payload["delivery_urgency"] = json!(urgency);

      let resp = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", self.api_key))
        .json(&payload)
        .send()
        .await;
      match resp {
        // 202 is a new event and 200 an idempotent duplicate
        Ok(r) if matches!(r.status().as_u16(), 200 | 202) => tracing::info!("Sent Trigv notification to {}.", channel),
        Ok(r) => {
          let status = r.status().as_u16();
          tracing::warn!("Failed to send Trigv notification to {}: {}error={}.", channel, error_hint(status), status);
          tracing::debug!("Response Details: {}", r.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Trigv notification to {}: {}", channel, e.without_url());
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

/// Trigv's own explanation of an HTTP status, for the log.
fn error_hint(status: u16) -> &'static str {
  match status {
    401 => "Unauthorized - Invalid API key., ",
    403 => "Forbidden - Revoked key, missing scope, or inactive workspace., ",
    404 => "Not found - Channel does not exist., ",
    422 => "Validation error - Check payload fields., ",
    429 => "Rate limit exceeded., ",
    _ => "",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::Value;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// The POSTs this test sent; other processes probing localhost ports can
  /// reach a mock server too, so anything else is ignored.
  async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "POST").collect()
  }

  const KEY: &str = "trgv_a1b2c3d4_0123456789abcdef0123456789abcdef";

  fn parse(url: &str) -> Option<Trigv> {
    Trigv::from_url(&ParsedUrl::parse(url)?)
  }

  fn bodies(requests: &[wiremock::Request]) -> Vec<Value> {
    requests.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  async fn send_ok(url: &str, ctx: &NotifyContext) -> (bool, Vec<wiremock::Request>) {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let svc = from_url(url).unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(ctx)).await.unwrap();
    (ok, received(&server).await)
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("trigvs://{}", KEY),
      format!("trigv://{}", KEY),
      format!("trigvs://{}/general", KEY),
      format!("trigvs://{}/deploys", KEY),
      format!("trigv://{}@trigv-platform.test/general", KEY),
      format!("trigv://{}@trigv-platform.test:8080/general", KEY),
      format!("trigvs://{}/alerts/ops", KEY),
      format!("trigvs://{}/?to=alerts,ops", KEY),
      format!("trigvs://{}/?url=https://example.com", KEY),
      format!("trigvs://{}/?urgency=time_sensitive", KEY),
      format!("trigvs://{}/?urgency=TIME_SENSITIVE&priority=invalid", KEY),
      format!("trigvs://{}/{}", KEY, "a".repeat(120)),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "trigvs://".to_string(),
      "trigv://".to_string(),
      "trigvs://not-a-valid-key".to_string(),
      format!("trigvs://{}x", KEY),
      format!("trigvs://{}/bad.channel/", KEY),
      format!("trigvs://{}/-leading", KEY),
      format!("trigvs://{}/{}", KEY, "a".repeat(121)),
      format!("trigvs://{}/?urgency=critical", KEY),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    let obj = parse(&format!("trigvs://{}/alerts", KEY)).unwrap();
    assert_eq!(obj.channels, vec!["alerts"]);
    assert!(obj.host.is_none());
    assert!(obj.secure);

    // Targets are de-duplicated and sorted, as upstream parse_list() does
    let obj = parse(&format!("trigvs://{}/alerts/?to=ops,escalation,alerts", KEY)).unwrap();
    assert_eq!(obj.channels, vec!["alerts", "escalation", "ops"]);

    // No channel falls back to the default one
    assert_eq!(parse(&format!("trigvs://{}", KEY)).unwrap().channels, vec![DEFAULT_CHANNEL]);

    let obj = parse(&format!("trigvs://{}@trigv-platform.test:8443/general", KEY)).unwrap();
    assert_eq!(obj.api_key, KEY);
    assert_eq!(obj.host.as_deref(), Some("trigv-platform.test"));
    assert_eq!(obj.port, Some(8443));
    assert_eq!(obj.notify_url(), "https://trigv-platform.test:8443/api/v1/events");
    let obj = parse(&format!("trigv://{}@trigv-platform.test/general", KEY)).unwrap();
    assert_eq!(obj.notify_url(), "http://trigv-platform.test/api/v1/events");

    let obj = parse(&format!(
      "trigvs://{}/deploys?url=https://example.com/run/1&image_url=https://example.com/image.png&urgency=time_sensitive&event_type=deploy.completed&priority=2",
      KEY
    ))
    .unwrap();
    assert_eq!(obj.supplemental_url.as_deref(), Some("https://example.com/run/1"));
    assert_eq!(obj.image_url.as_deref(), Some("https://example.com/image.png"));
    assert_eq!(obj.urgency, "time_sensitive");
    assert_eq!(obj.event_type.as_deref(), Some("deploy.completed"));
    assert_eq!(obj.priority, Some(2));
    assert_eq!(parse(&format!("trigvs://{}?priority=invalid", KEY)).unwrap().priority, None);
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url(&format!("trigvs://{}", KEY)).unwrap();
    assert_eq!(svc.body_maxlen(), 1000);
    assert_eq!(svc.title_maxlen(), 255);
    assert!(!svc.attachment_support());
    assert_eq!(Trigv::static_details().protocols, vec!["trigv", "trigvs"]);
  }

  #[tokio::test]
  async fn test_send_general() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/events"))
      .and(header("User-Agent", APP_ID))
      .and(header("Accept", "application/json"))
      .and(header("Content-Type", "application/json"))
      .and(header("Authorization", format!("Bearer {}", KEY).as_str()))
      .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"event":{"public_id":"evt_test","status":"queued"}}"#))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("trigvs://{}/alerts", KEY)).unwrap();
    let ctx = NotifyContext { title: "Cron".into(), body: "Backup failed".into(), notify_type: NotifyType::Failure, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let payloads = bodies(&received(&server).await);
    assert_eq!(
      payloads[0],
      json!({ "channel": "alerts", "title": "Cron", "level": "error", "description": "Backup failed", "delivery_urgency": "time_sensitive" })
    );
  }

  #[tokio::test]
  async fn test_send_multiple_channels_with_extras() {
    // Matches pycapture for the same URL: one request per sorted channel
    let url = format!("trigvs://{}/zeta/alpha?to=beta,alpha&url=https://x.io&image_url=https://i.io/a.png&event_type=deploy&priority=1", KEY);
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    let (ok, requests) = send_ok(&url, &ctx).await;
    assert!(ok);
    let payloads = bodies(&requests);
    let channels: Vec<&str> = payloads.iter().map(|p| p["channel"].as_str().unwrap()).collect();
    assert_eq!(channels, vec!["alpha", "beta", "zeta"]);
    assert_eq!(
      payloads[0],
      json!({
        "channel": "alpha",
        "title": "T",
        "level": "info",
        "description": "B",
        "url": "https://x.io",
        "image_url": "https://i.io/a.png",
        "event_type": "deploy",
        "delivery_urgency": "time_sensitive",
      })
    );
  }

  #[tokio::test]
  async fn test_send_title_fallbacks() {
    // No title: the app description stands in, and fills an empty body too
    let ctx = NotifyContext::default();
    let (_, requests) = send_ok(&format!("trigvs://{}", KEY), &ctx).await;
    let payload = &bodies(&requests)[0];
    assert_eq!(payload["title"], "Apprise Notifications");
    assert_eq!(payload["description"], "Apprise Notifications");
    assert_eq!(payload["channel"], "general");
    assert_eq!(payload["delivery_urgency"], "standard");

    // A title with no body omits the description
    let ctx = NotifyContext { title: "Title".into(), ..Default::default() };
    let (_, requests) = send_ok(&format!("trigvs://{}?priority=invalid", KEY), &ctx).await;
    let payload = &bodies(&requests)[0];
    assert!(payload.get("description").is_none());
    assert_eq!(payload["level"], "info");
    assert_eq!(payload["delivery_urgency"], "standard");

    // Title and body are capped to the payload limits
    let ctx = NotifyContext { title: "t".repeat(300), body: "b".repeat(1200), ..Default::default() };
    let (_, requests) = send_ok(&format!("trigvs://{}", KEY), &ctx).await;
    let payload = &bodies(&requests)[0];
    assert_eq!(payload["title"].as_str().unwrap().len(), 255);
    assert_eq!(payload["description"].as_str().unwrap().len(), 1000);
  }

  #[tokio::test]
  async fn test_send_urgency() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type: NotifyType::Warning, ..Default::default() };
    for (query, expected) in [("", "standard"), ("?priority=0", "standard"), ("?priority=1", "time_sensitive"), ("?urgency=time_sensitive", "time_sensitive")] {
      let (_, requests) = send_ok(&format!("trigvs://{}{}", KEY, query), &ctx).await;
      assert_eq!(bodies(&requests)[0]["delivery_urgency"], expected, "{}", query);
    }
    for (notify_type, level) in [(NotifyType::Success, "success"), (NotifyType::Warning, "warning")] {
      let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type, ..Default::default() };
      let (_, requests) = send_ok(&format!("trigvs://{}", KEY), &ctx).await;
      assert_eq!(bodies(&requests)[0]["level"], level);
    }
  }

  #[tokio::test]
  async fn test_send_custom_host() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v1/events")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let addr = server.address();
    let svc = from_url(&format!("trigv://{}@{}:{}/general", KEY, addr.ip(), addr.port())).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_partial_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let svc = from_url(&format!("trigvs://{}/alerts/ops", KEY)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // Every channel is still tried
    assert_eq!(received(&server).await.len(), 2);
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [401u16, 422, 999, 204] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("trigvs://{}", KEY)).unwrap();
      let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "status {}", status);
    }
    let svc = from_url(&format!("trigvs://{}/a/b", KEY)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
