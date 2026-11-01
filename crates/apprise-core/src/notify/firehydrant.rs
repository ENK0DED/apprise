//! FireHydrant Signals, through a generic webhook event source:
//!
//!   firehydrant://{token}
//!
//! The token is the end of the webhook URL shown under "Signals" ->
//! "Sources" (or on a team's "Alert Triggers" tab), which Apprise also
//! accepts as-is: `https://signals.firehydrant.com/v1/process/{token}`.
//!
//! See <https://docs.firehydrant.com/docs/generic-webhook-event-source> and
//! <https://docs.firehydrant.com/docs/events-data-model>.

use crate::asset::NotifyImageSize;
use crate::conversion::pystr;
use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_list, unquote};
use crate::utils::plugin_args::qsd_bool;
use async_trait::async_trait;
use serde_json::{Map, Value, json};

/// Upstream `FIREHYDRANT_ACTIONS`, in the order a partial `?action=` is
/// matched. `map` resolves on success and opens an alert otherwise.
const ACTIONS: &[&str] = &["map", "trigger", "resolve"];

/// Upstream `FIREHYDRANT_LEVELS`.
const LEVELS: &[&str] = &["info", "warn", "error", "fatal"];

/// Upstream `FIREHYDRANT_PRIORITIES`.
const PRIORITIES: &[&str] = &["low", "medium", "high"];

/// The annotation FireHydrant reads to decide how hard to page someone.
const PRIORITY_ANNOTATION: &str = "signals.firehydrant.com/notification-priority";

/// Upstream `FIREHYDRANT_HTTP_ERROR_MAP`.
const HTTP_ERRORS: &[(u16, &str)] = &[
  (400, "The event was rejected as invalid."),
  (401, "Unauthorized - Invalid Token."),
  (404, "The webhook token could not be found."),
  (429, "Too many events were sent."),
];

/// Upstream `title_maxlen`; FireHydrant cuts the summary to this anyway.
const TITLE_MAXLEN: usize = 255;

pub struct FireHydrant {
  token: String,
  action: &'static str,
  level: Option<&'static str>,
  priority: Option<&'static str>,
  dedup: Option<String>,
  click: Option<String>,
  /// The event's own tags (`?tags=service:api`), which tie the alert to a
  /// catalog entry; not the routing tags.
  event_tags: Vec<String>,
  include_image: bool,
  annotations: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `validate_regex(token, r"^[A-Z0-9_.=-]+$", "i")`: Python's `$`
/// also matches before a final newline, and the match is returned stripped.
fn valid_token(value: &str) -> Option<String> {
  let core = value.strip_suffix('\n').unwrap_or(value);
  let ok = !core.is_empty() && core.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '=' | '-'));
  ok.then(|| pystr::strip(value).to_string())
}

/// Upstream `_choice()`: `Some(None)` when the argument is missing or blank,
/// `Some(choice)` for the first of `values` it starts (so `r` picks
/// `resolve`), and `None` (the URL is refused) for anything else.
fn choice(url: &ParsedUrl, name: &str, values: &[&'static str]) -> Option<Option<&'static str>> {
  let Some(value) = arg(url, name) else { return Some(None) };
  let wanted = pystr::strip(&value).to_lowercase();
  if wanted.is_empty() {
    return Some(None);
  }
  match incident::match_prefix(values, &wanted) {
    Some(choice) => Some(Some(choice)),
    None => {
      tracing::warn!("The FireHydrant {} specified ({}) is invalid.", name, value);
      None
    }
  }
}

/// An optional argument, stripped, with blank values left out.
fn stripped(url: &ParsedUrl, key: &str) -> Option<String> {
  arg(url, key).map(|v| pystr::strip(&v).to_string()).filter(|v| !v.is_empty())
}

impl FireHydrant {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The token is the host, or ?token= for an easier YAML configuration
    let raw = arg(url, "token").unwrap_or_else(|| unquote(url.host.as_deref().unwrap_or_default()));
    let Some(token) = valid_token(&raw) else {
      tracing::warn!("An invalid FireHydrant token was specified.");
      return None;
    };

    Some(Self {
      token,
      action: choice(url, "action", ACTIONS)?.unwrap_or(ACTIONS[0]),
      level: choice(url, "level", LEVELS)?,
      priority: choice(url, "priority", PRIORITIES)?,
      // A fixed key lets several notifications update the same alert
      dedup: stripped(url, "dedup"),
      // A link back to wherever the alert came from
      click: stripped(url, "click"),
      event_tags: arg(url, "tags").map(|t| parse_list(&t)).unwrap_or_default(),
      include_image: qsd_bool(url, "image", true),
      // Extra details the alert rules can match on
      annotations: incident::unquoted_map(&url.qsd_add),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "FireHydrant",
      service_url: Some("https://firehydrant.com/"),
      setup_url: Some("https://appriseit.com/services/firehydrant/"),
      protocols: vec!["firehydrant"],
      description: "Open and resolve FireHydrant Signals alerts through a webhook event source.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Work out whether we open or resolve the alert
    let resolve = self.action == "resolve" || (self.action == "map" && ctx.notify_type == NotifyType::Success);

    // Our own key always wins. A trigger opens its own alert each time (the
    // key is kept across retries); map and resolve need a key that stays the
    // same between notifications, taken from the title.
    let dedup = match &self.dedup {
      Some(key) => key.clone(),
      None if self.action == "trigger" => incident::retry_key("dedup"),
      None => incident::title_key(&ctx.title, &ctx.body),
    };

    let level = self.level.unwrap_or(match ctx.notify_type {
      NotifyType::Warning => "warn",
      NotifyType::Failure => "error",
      _ => "info",
    });

    // A summary is required, so the body stands in for a missing title
    let summary: String = if ctx.title.is_empty() { ctx.body.chars().take(TITLE_MAXLEN).collect() } else { ctx.title.clone() };
    let mut payload = json!({
      "summary": summary,
      "body": ctx.body,
      "level": level.to_uppercase(),
      "status": if resolve { "CLOSED" } else { "OPEN" },
      "idempotency_key": dedup,
    });

    // The priority annotation goes first so +key=value can replace it
    let mut annotations = Map::new();
    if let Some(priority) = self.priority {
      annotations.insert(PRIORITY_ANNOTATION.into(), json!(priority.to_uppercase()));
    }
    for (k, v) in &self.annotations {
      annotations.insert(k.clone(), json!(v));
    }
    if !annotations.is_empty() {
      payload["annotations"] = Value::Object(annotations);
    }
    if !self.event_tags.is_empty() {
      payload["tags"] = json!(self.event_tags);
    }
    if let Some(click) = &self.click {
      payload["links"] = json!([{ "href": click, "text": click }]);
    }
    if self.include_image
      && let Some(image) = ctx.image_url(NotifyImageSize::Xy256)
    {
      payload["images"] = json!([{ "src": image, "alt": ctx.notify_type.as_str() }]);
    }
    payload
  }
}

#[async_trait]
impl Notify for FireHydrant {
  fn schemas(&self) -> &[&str] {
    &["firehydrant"]
  }
  fn service_name(&self) -> &str {
    "FireHydrant"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    TITLE_MAXLEN
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    // The webhook token identifies the connection
    Some(vec!["firehydrant".into(), self.token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = self.payload(ctx);
    // The URL carries the token, so only the payload is logged
    tracing::debug!("FireHydrant Payload: {}", payload);

    let client = build_client(self.verify_certificate)?;
    let request =
      client.post(format!("{}/v1/process/{}", service_base("https://signals.firehydrant.com"), self.token)).header("User-Agent", APP_ID).json(&payload);
    incident::exchange(request, "FireHydrant event", &[200, 201, 202], HTTP_ERRORS).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyFormat;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// A realistic (JWT shaped) webhook token, as upstream's tests use.
  const TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJvcmdhbml6YXRpb25faWQiOiJhYmMtMTIzIn0.Ab-Cd_EfGhIjKlMnOpQrStUvWxYz0123456789";
  const FAILURE_IMAGE: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-failure-256x256.png";

  fn firehydrant(url: &str) -> FireHydrant {
    FireHydrant::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  fn process_path(token: &str) -> String {
    format!("/v1/process/{token}")
  }

  /// The JSON bodies posted to the webhook of `token`, in order.
  async fn sent(server: &MockServer, token: &str) -> Vec<Value> {
    let p = process_path(token);
    let requests = server.received_requests().await.unwrap_or_default();
    requests.iter().filter(|r| r.method.as_str() == "POST" && r.url.path() == p).map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  #[test]
  fn test_valid_urls() {
    // Upstream test_plugin_firehydrant_urls
    let urls = [
      format!("firehydrant://{TOKEN}"),
      format!("firehydrant://?token={TOKEN}"),
      format!(
        "firehydrant://{TOKEN}?action=resolve&level=fatal&priority=low&dedup=disk-full&click=https://example.com/alert\
         &tags=service:api,env:prod&image=no&+team=sre"
      ),
      format!("https://signals.firehydrant.com/v1/process/{TOKEN}"),
      format!("https://signals.firehydrant.com/v1/process/{TOKEN}/?priority=high"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "firehydrant://".to_string(),
      "firehydrant://invalid!token".to_string(),
      format!("firehydrant://{TOKEN}?action=invalid"),
      format!("firehydrant://{TOKEN}?level=invalid"),
      format!("firehydrant://{TOKEN}?priority=invalid"),
      "firehydrant://?token=bad!".to_string(),
      // Each checked with upstream's Apprise.instantiate()
      "https://signals.firehydrant.com/v1/process/abc/extra".to_string(),
      "https://signals.firehydrant.com/v1/process/abc?".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Each row matches what upstream's Apprise.instantiate() set
    let obj = firehydrant(&format!(
      "firehydrant://{TOKEN}?action=resolve&level=fatal&priority=low&dedup=disk-full&click=https://example.com/alert\
       &tags=service:api,env:prod&image=no&+team=sre"
    ));
    assert_eq!(obj.token, TOKEN);
    assert_eq!((obj.action, obj.level, obj.priority), ("resolve", Some("fatal"), Some("low")));
    assert_eq!(obj.dedup.as_deref(), Some("disk-full"));
    assert_eq!(obj.click.as_deref(), Some("https://example.com/alert"));
    assert_eq!(obj.event_tags, vec!["env:prod", "service:api"]);
    assert!(!obj.include_image);
    assert_eq!(obj.annotations, vec![("team".to_string(), "sre".to_string())]);

    let obj = firehydrant(&format!("firehydrant://{TOKEN}"));
    assert_eq!((obj.action, obj.level, obj.priority), ("map", None, None));
    assert!(obj.include_image && obj.dedup.is_none() && obj.click.is_none() && obj.event_tags.is_empty() && obj.annotations.is_empty());

    // Partial and mixed-case choices; blank ones are unset instead of
    // matching the first choice
    let obj = firehydrant("firehydrant://abc?action=T&level=W&priority=H");
    assert_eq!((obj.action, obj.level, obj.priority), ("trigger", Some("warn"), Some("high")));
    let obj = firehydrant("firehydrant://abc?action=%20&level=%20%20&priority=");
    assert_eq!((obj.action, obj.level, obj.priority), ("map", None, None));
    assert_eq!(firehydrant("firehydrant://abc?ACTION=Resolve").action, "resolve");
    assert_eq!(firehydrant("firehydrant://abc?action=resolve%20").action, "resolve");

    // ?token= replaces the host; an empty one does not
    assert_eq!(firehydrant("firehydrant://abc?token=other").token, "other");
    assert_eq!(firehydrant("firehydrant://abc?token=").token, "abc");
    // Base64 padding and every allowed character; credentials, port and
    // path are ignored
    assert_eq!(firehydrant("firehydrant://abc==").token, "abc==");
    assert_eq!(firehydrant("firehydrant://abc%3D%3D").token, "abc==");
    assert_eq!(firehydrant("firehydrant://a_b-c.d=e?image=yes").token, "a_b-c.d=e");
    assert_eq!(firehydrant("firehydrant://user:pw@abc:123/extra/path").token, "abc");
    // Python's `$` lets one final newline through, then strips it
    assert_eq!(firehydrant("firehydrant://ab%0A").token, "ab");
    assert_eq!(firehydrant("firehydrant://?token=ab%0A").token, "ab");

    // Blank dedup and click are unset; tags are split, sorted and made
    // unique; an unrecognised image value is false
    let obj = firehydrant("firehydrant://abc?dedup=%20%20&click=%20&tags=b%20a,,c;a&image=maybe");
    assert!(obj.dedup.is_none() && obj.click.is_none());
    assert_eq!(obj.event_tags, vec!["a", "b", "c"]);
    assert!(!obj.include_image);

    // Annotations are unquoted a second time, as upstream does
    let obj = firehydrant("firehydrant://abc?+a%2520=b%2520&+x=");
    assert_eq!(obj.annotations, vec![("a ".to_string(), "b ".to_string()), ("x".to_string(), String::new())]);
  }

  #[test]
  fn test_native_urls() {
    let obj = firehydrant(&format!("firehydrant://{TOKEN}"));
    for url in [
      format!("https://signals.firehydrant.com/v1/process/{TOKEN}"),
      format!("http://signals.firehydrant.com/v1/process/{TOKEN}/"),
      format!("HTTPS://SIGNALS.FIREHYDRANT.COM/V1/PROCESS/{TOKEN}?Level=Error"),
    ] {
      let parsed = crate::notify::registry::parse_service_url(&url).unwrap();
      let native = FireHydrant::from_url(&parsed).unwrap();
      assert_eq!(native.token, obj.token, "{url}");
    }
    let parsed = crate::notify::registry::parse_service_url(&format!("https://signals.firehydrant.com/v1/process/{TOKEN}/?priority=high")).unwrap();
    assert_eq!(FireHydrant::from_url(&parsed).unwrap().priority, Some("high"));
    let parsed = crate::notify::registry::parse_service_url("https://signals.firehydrant.com/v1/process/abc?Level=Error").unwrap();
    assert_eq!(FireHydrant::from_url(&parsed).unwrap().level, Some("error"));
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; only the token counts
    crate::notify::custom_json::assert_upstream_uids(&[
      ("firehydrant://eyJhbGciOiJIUzI1NiJ9.eyJvcmdhbml6YXRpb25faWQiOiJhYmMtMTIzIn0.Ab-Cd_EfGhIjKlMnOpQrStUvWxYz0123456789", "c1ec9c60"),
      ("firehydrant://?token=eyJhbGciOiJIUzI1NiJ9.eyJvcmdhbml6YXRpb25faWQiOiJhYmMtMTIzIn0.Ab-Cd_EfGhIjKlMnOpQrStUvWxYz0123456789", "c1ec9c60"),
      (
        "https://signals.firehydrant.com/v1/process/eyJhbGciOiJIUzI1NiJ9.eyJvcmdhbml6YXRpb25faWQiOiJhYmMtMTIzIn0.Ab-Cd_EfGhIjKlMnOpQrStUvWxYz0123456789",
        "c1ec9c60",
      ),
      ("firehydrant://abc.def.ghi", "64f6ce5a"),
      ("firehydrant://abc.def.ghi?action=trigger&priority=high&+team=sre", "64f6ce5a"),
      ("firehydrant://abc==", "749f23f7"),
    ]);
  }

  #[tokio::test]
  async fn test_send_payload() {
    // Upstream test_plugin_firehydrant_payload, checked with pycapture.py
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(process_path(TOKEN)))
      .and(header("content-type", "application/json"))
      .and(header("user-agent", APP_ID))
      .and(body_json(json!({
        "summary": "Disk & <db01>",
        "body": "Usage at 95% on _db01_ * # <",
        "level": "FATAL",
        "status": "CLOSED",
        "idempotency_key": "disk-full",
        "annotations": {
          "signals.firehydrant.com/notification-priority": "MEDIUM",
          "team": "sre",
        },
        "tags": ["env:prod", "service:api"],
        "links": [{"href": "https://example.com/alert", "text": "https://example.com/alert"}],
        "images": [{"src": FAILURE_IMAGE, "alt": "failure"}],
      })))
      .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"success": true}"#))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!(
      "firehydrant://{TOKEN}?action=res&level=fa&priority=m&dedup=disk-full&click=https://example.com/alert&tags=service:api,env:prod&+team=sre"
    ))
    .unwrap();
    let c = ctx("Disk & <db01>", "Usage at 95% on _db01_ * # <", NotifyType::Failure);
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
    // One post to the webhook, with no other credentials
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().filter(|r| r.url.path() == process_path(TOKEN)).all(|r| !r.headers.contains_key("authorization")));
  }

  #[test]
  fn test_payload_details() {
    // Our own annotation can replace the priority one
    let obj = firehydrant("firehydrant://abc?priority=high&+signals.firehydrant.com/notification-priority=LOW");
    let p = obj.payload(&ctx("", "test", NotifyType::Info));
    assert_eq!(p["annotations"], json!({PRIORITY_ANNOTATION: "LOW"}));

    // Without a title, the body stands in for the summary and is capped;
    // nothing optional is sent
    let obj = firehydrant("firehydrant://abc?image=no");
    let p = obj.payload(&ctx("", &"x".repeat(300), NotifyType::Info));
    assert_eq!(p["summary"], "x".repeat(255));
    assert_eq!(p["body"], "x".repeat(300));
    for key in ["annotations", "tags", "links", "images"] {
      assert!(p.get(key).is_none(), "{key}");
    }
    // The summary is cut in characters
    assert_eq!(obj.payload(&ctx("", &"é".repeat(300), NotifyType::Info))["summary"], "é".repeat(255));
  }

  #[test]
  fn test_levels() {
    // Upstream test_plugin_firehydrant_level
    let obj = firehydrant("firehydrant://abc");
    for (notify_type, level) in [(NotifyType::Info, "INFO"), (NotifyType::Success, "INFO"), (NotifyType::Warning, "WARN"), (NotifyType::Failure, "ERROR")] {
      assert_eq!(obj.payload(&ctx("", "test", notify_type))["level"], level);
    }
    // A level that was set always wins
    assert_eq!(firehydrant("firehydrant://abc?level=WARN").payload(&ctx("", "test", NotifyType::Failure))["level"], "WARN");
  }

  #[test]
  fn test_actions() {
    // Upstream test_plugin_firehydrant_action_map
    let sent = |obj: &FireHydrant, title: &str, body: &str, notify_type| obj.payload(&ctx(title, body, notify_type));

    // The default action maps the notification type
    let obj = firehydrant("firehydrant://abc");
    assert_eq!(obj.action, "map");
    for (notify_type, status) in [(NotifyType::Info, "OPEN"), (NotifyType::Warning, "OPEN"), (NotifyType::Failure, "OPEN"), (NotifyType::Success, "CLOSED")] {
      assert_eq!(sent(&obj, "Disk full", "db01", notify_type)["status"], status);
    }

    // The same title always produces the same key, so a success resolves
    // the alert a failure opened
    let opened = sent(&obj, "Disk full", "a", NotifyType::Failure)["idempotency_key"].clone();
    assert_eq!(opened, incident::sha256_hex("Disk full"));
    assert_eq!(sent(&obj, "Disk full", "b", NotifyType::Success)["idempotency_key"], opened);
    // A different title is a different alert
    assert_ne!(sent(&obj, "CPU hot", "a", NotifyType::Info)["idempotency_key"], opened);
    // Without a title, the first line of the message is used
    assert_eq!(sent(&obj, "", "Disk full\nat 95%", NotifyType::Info)["idempotency_key"], opened);

    // Trigger ignores the notification type and opens a new alert every
    // time, even with the same title
    let obj = firehydrant("firehydrant://abc?action=trigger");
    let first = sent(&obj, "Disk full", "ok", NotifyType::Success);
    assert_eq!(first["status"], "OPEN");
    assert!(crate::utils::plugin_args::is_uuid4(first["idempotency_key"].as_str().unwrap()));
    assert_ne!(sent(&obj, "Disk full", "ok", NotifyType::Info)["idempotency_key"], first["idempotency_key"]);

    // Resolve ignores the notification type
    let p = sent(&firehydrant("firehydrant://abc?action=r"), "Disk full", "bad", NotifyType::Failure);
    assert_eq!(p["status"], "CLOSED");
    assert_eq!(p["idempotency_key"], opened);
  }

  #[tokio::test]
  async fn test_formats_through_apprise() {
    // Matches pycapture.py: the body is sent as text (HTML is converted,
    // Markdown and text are left alone) and stands in for the summary
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(process_path("abc.def.ghi"))).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    for (body, format, expected) in
      [("**bold**_x_", NotifyFormat::Markdown, "**bold**_x_"), ("<b>bold</b>&amp;x", NotifyFormat::Html, "bold&x"), ("a*b_c", NotifyFormat::Text, "a*b_c")]
    {
      let mut a = crate::Apprise::new();
      assert!(a.add("firehydrant://abc.def.ghi?image=no"));
      let c = NotifyContext { body_format: format, ..ctx("", body, NotifyType::Info) };
      assert!(crate::notify::with_test_base(server.uri(), a.notify(&c)).await.is_success(), "{body}");
      let last = sent(&server, "abc.def.ghi").await.pop().unwrap();
      assert_eq!(
        last,
        json!({
          "summary": expected,
          "body": expected,
          "level": "INFO",
          "status": "OPEN",
          "idempotency_key": incident::sha256_hex(expected),
        }),
        "{body}"
      );
    }

    // A passthrough body reaches FireHydrant untouched
    let mut a = crate::Apprise::new();
    assert!(a.add("firehydrant://abc.def.ghi?image=no"));
    let c = NotifyContext { body_passthrough: true, ..ctx("", "**bold** & <b>html</b>", NotifyType::Info) };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&c)).await.is_success());
    assert_eq!(sent(&server, "abc.def.ghi").await.pop().unwrap()["body"], "**bold** & <b>html</b>");
  }

  #[tokio::test]
  async fn test_retry_keeps_key() {
    // Upstream test_plugin_firehydrant_retry_key: the retry reuses the
    // trigger's key so FireHydrant sees one alert
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(process_path(TOKEN)))
      .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"errors": ["summary is required"]}"#))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(process_path(TOKEN))).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("firehydrant://{TOKEN}?action=trigger&retry=1&wait=0")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server down", NotifyType::Info))).await.is_success());
    let keys: Vec<Value> = sent(&server, TOKEN).await.into_iter().map(|p| p["idempotency_key"].clone()).collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1]);
  }

  #[tokio::test]
  async fn test_send_statuses() {
    // pycapture.py: 200, 201 and 202 are accepted, anything else fails
    for status in [200, 201, 202, 204, 400, 401, 404, 429, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(process_path(TOKEN))).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("firehydrant://{TOKEN}")).unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b", NotifyType::Info))).await;
      if [200, 201, 202].contains(&status) {
        assert!(result.unwrap(), "{status}");
      } else {
        assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status), "{status}");
      }
    }
    assert_eq!(incident::status_detail(404, HTTP_ERRORS), "The webhook token could not be found., error=404");
  }

  #[tokio::test]
  async fn test_connection_error() {
    let svc = from_url("firehydrant://abc").unwrap();
    let result = crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("t", "b", NotifyType::Info))).await;
    assert!(matches!(result, Err(NotifyError::Http(_))));
  }

  #[test]
  fn test_service_details() {
    let svc = from_url("firehydrant://abc").unwrap();
    assert_eq!(svc.service_name(), "FireHydrant");
    assert_eq!(svc.notify_format(), NotifyFormat::Text);
    assert_eq!(svc.title_maxlen(), 255);
    assert_eq!(svc.body_maxlen(), 32768);
    assert!(!svc.attachment_support());
    assert_eq!(FireHydrant::static_details().protocols, vec!["firehydrant"]);
    // A different token is a different connection
    assert_ne!(firehydrant("firehydrant://abc.def.ghi").url_identifier(), firehydrant("firehydrant://abc").url_identifier());
  }
}
