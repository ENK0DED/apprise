//! GoAlert, the self-hosted on-call platform, through its Generic API:
//!
//!   goalert(s)://{host}[:{port}][/{path}]/{integration_key}[/{key2}...]
//!
//! Each integration key (a UUID) belongs to one GoAlert service; any other
//! path entries are where GoAlert is hosted on the server. See
//! <https://github.com/target/goalert>.

use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::{NotifyFormat, NotifyType};
use crate::utils::parse::{ParsedUrl, URL_PATH_SAFE_CHARS, quote};
use crate::utils::plugin_args::parse_list;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `GOALERT_ACTIONS`, in the order a partial `?action=` is matched.
const ACTIONS: &[&str] = &["map", "trigger", "close"];

/// The Generic API endpoint, relative to the GoAlert server and path.
const API_PATH: &str = "api/v2/generic/incoming";

/// Upstream `GOALERT_HTTP_ERROR_MAP`.
const HTTP_ERRORS: &[(u16, &str)] = &[
  (400, "Bad Request - Invalid payload."),
  (401, "Unauthorized - Invalid Integration Key."),
  (403, "Forbidden - Invalid Integration Key."),
  (429, "Too many requests were made."),
];

/// The alert summary is sent as SMS and voice, and is limited to 1KiB.
const SUMMARY_MAXLEN: usize = 1024;

/// Upstream `IS_INTEGRATION_KEY`: a UUID, optionally padded with whitespace.
fn integration_key(entry: &str) -> Option<String> {
  let key = entry.trim();
  let parts: Vec<&str> = key.split('-').collect();
  let ok = parts.len() == 5 && [8, 4, 4, 4, 12].iter().zip(&parts).all(|(len, part)| part.len() == *len && part.chars().all(|c| c.is_ascii_hexdigit()));
  ok.then(|| key.to_string())
}

pub struct GoAlert {
  secure: bool,
  host: String,
  port: Option<u16>,
  /// Where GoAlert is hosted: the path entries that are not keys, joined.
  fullpath: String,
  targets: Vec<String>,
  action: &'static str,
  dedup: Option<String>,
  meta: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl GoAlert {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.verified_host()?;

    // Integration keys are UUIDs, so anything else in the path is where
    // GoAlert is hosted on the server
    let mut entries: Vec<String> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    for entry in &url.path_parts {
      if integration_key(entry).is_some() { entries.push(entry.clone()) } else { path.push(entry.clone()) }
    }
    for key in ["to", "token"] {
      if let Some(value) = arg(url, key) {
        entries.extend(parse_list([value], true, true));
      }
    }

    let action = match arg(url, "action") {
      Some(a) => match incident::match_choice(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The GoAlert action specified ({}) is invalid.", a);
          return None;
        }
      },
      None => "map",
    };

    let mut targets = Vec::new();
    for entry in parse_list(entries, true, true) {
      match integration_key(&entry) {
        // Stored in lower case so duplicates line up
        Some(key) => targets.push(key.to_lowercase()),
        None => tracing::warn!("Ignoring invalid GoAlert integration key ({}).", entry),
      }
    }

    Some(Self {
      secure: url.schema == "goalerts",
      host,
      port: url.port,
      fullpath: path.join("/"),
      targets,
      action,
      // An optional key so repeated notifications update one alert
      dedup: arg(url, "dedup").map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
      meta: incident::unquoted_map(&url.qsd_add),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "GoAlert",
      service_url: Some("https://goalert.me/"),
      setup_url: Some("https://appriseit.com/services/goalert/"),
      protocols: vec!["goalert", "goalerts"],
      description: "Raise and close GoAlert alerts through the Generic API.",
      attachment_support: false,
    }
  }

  fn endpoint(&self) -> String {
    format!(
      "{}://{}{}/{}{}",
      if self.secure { "https" } else { "http" },
      self.host,
      self.port.map(|p| format!(":{}", p)).unwrap_or_default(),
      if self.fullpath.is_empty() { String::new() } else { quote(&format!("{}/", self.fullpath), URL_PATH_SAFE_CHARS) },
      API_PATH
    )
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Work out whether we raise or close the alert
    let close = self.action == "close" || (self.action == "map" && ctx.notify_type == NotifyType::Success);
    // The summary is plain text sent by SMS and voice. A body with no input
    // format has no escapes Apprise added, so it is used as written.
    let summary = match ctx.body_passthrough {
      true if ctx.title.is_empty() => ctx.body.chars().take(SUMMARY_MAXLEN).collect(),
      _ => incident::plain_title(&ctx.title, &ctx.body, SUMMARY_MAXLEN),
    };
    let mut payload = json!({
      "summary": summary,
      "details": ctx.body,
    });
    if close {
      payload["action"] = json!("close");
    }
    if let Some(dedup) = &self.dedup {
      payload["dedup"] = json!(dedup);
    }
    if !self.meta.is_empty() {
      payload["meta"] = incident::json_object(&self.meta);
    }
    payload
  }
}

#[async_trait]
impl Notify for GoAlert {
  fn schemas(&self) -> &[&str] {
    &["goalert", "goalerts"]
  }
  fn service_name(&self) -> &str {
    "GoAlert"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // GoAlert renders the alert details as markdown
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn title_maxlen(&self) -> usize {
    SUMMARY_MAXLEN
  }
  // The alert details are limited to 6KiB
  fn body_maxlen(&self) -> usize {
    6144
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    let port = self.port.unwrap_or(if self.secure { 443 } else { 80 });
    Some(vec![if self.secure { "goalerts" } else { "goalert" }.into(), self.host.clone(), port.to_string(), self.fullpath.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no GoAlert integration keys to notify.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let url = self.endpoint();
    let payload = self.payload(ctx);
    let mut has_error = false;
    for key in &self.targets {
      // Skip a key that already accepted this alert
      if crate::delivery::is_delivered(key, false) {
        continue;
      }
      // The key is passed as a bearer token, never in the URL
      let request = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", key)).json(&payload);
      match incident::exchange(request, "GoAlert notification", &[200, 201, 202, 204], HTTP_ERRORS).await {
        Ok(reply) if reply.delivered => crate::delivery::mark_delivered(key, false),
        Ok(_) => has_error = true,
        Err(e) => {
          incident::log_connection_error("GoAlert notification", &e);
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
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const KEY1: &str = "ab12cd34-ab12-4c5d-8e9f-0123456789ab";
  const KEY2: &str = "ab12cd34-ab12-4c5d-8e9f-0123456789ac";

  fn goalert(url: &str) -> GoAlert {
    GoAlert::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  /// The mock server's `host:port`.
  fn authority(server: &MockServer) -> String {
    server.uri().trim_start_matches("http://").to_string()
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      // No integration keys; loads but has nothing to notify
      "goalerts://localhost".to_string(),
      "goalerts://localhost/?to=invalid".to_string(),
      format!("goalert://localhost/{KEY1}"),
      format!("goalerts://localhost:8443/goalert/{KEY1}/{KEY2}"),
      format!("goalerts://localhost/?token={KEY1}&action=close&dedup=disk"),
      format!("goalerts://localhost/{KEY1}?action=trigger&+env=prod"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = ["goalert://".to_string(), format!("goalerts://localhost/{KEY1}?action=invalid"), format!("goalert://bad_host!/{KEY1}")];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = goalert(&format!("goalerts://localhost:8443/go%20alert/sub/{}/{KEY1}?to=bad-key&dedup=%20x%20&+env=prod", KEY2.to_uppercase()));
    assert!(obj.secure);
    assert_eq!(obj.port, Some(8443));
    assert_eq!(obj.fullpath, "go alert/sub");
    // Sorted as given, then lower-cased; bad keys are dropped
    assert_eq!(obj.targets, vec![KEY2, KEY1]);
    assert_eq!(obj.dedup.as_deref(), Some("x"));
    assert_eq!(obj.action, "map");
    assert_eq!(obj.endpoint(), "https://localhost:8443/go%20alert/sub/api/v2/generic/incoming");
    assert_eq!(goalert(&format!("goalert://localhost/{KEY1}")).endpoint(), "http://localhost/api/v2/generic/incoming");
    assert_eq!(goalert(&format!("goalert://localhost/{KEY1}?action=%20TRIG%20")).action, "trigger");
    assert_eq!(goalert(&format!("goalert://localhost/{KEY1}?action=c")).action, "close");
    assert_eq!(goalert(&format!("goalert://localhost/?token={KEY1}")).targets, vec![KEY1]);
    // Path entries that are not keys are where GoAlert is hosted
    assert_eq!(goalert(&format!("goalert://localhost:8080/a/b/{KEY1}")).fullpath, "a/b");
  }

  #[test]
  fn test_url_identifier() {
    // The server and its path identify the connection, not its keys
    let id = |url: &str| goalert(url).url_identifier();
    assert_eq!(id(&format!("goalerts://localhost/sub/{KEY1}")), id(&format!("goalerts://localhost/sub/{KEY2}")));
    assert_ne!(id(&format!("goalerts://localhost/sub/{KEY1}")), id(&format!("goalerts://localhost/{KEY1}")));
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/goalert/api/v2/generic/incoming"))
      .and(header("authorization", format!("Bearer {KEY1}")))
      .and(header("content-type", "application/json"))
      .and(body_json(json!({
        "summary": "Disk <full> & failing",
        "details": "Some _markdown_ text",
        "dedup": "disk",
        "meta": {"env": "prod", "team": "ops"},
      })))
      .respond_with(ResponseTemplate::new(204))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("goalert://{}/goalert/{KEY1}?dedup=disk&+env=prod&+team=ops", authority(&server))).unwrap();
    assert!(svc.send(&ctx("Disk <full> & failing", "Some _markdown_ text", NotifyType::Failure)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_each_key() {
    // Matches pycapture.py with --input-format text and --type success:
    // one request per key, in sorted order, closing the alert
    let server = MockServer::start().await;
    let body = json!({"summary": "50% done_now *", "details": "50% done\\_now \\*", "action": "close", "dedup": "disk", "meta": {"env": "prod"}});
    for key in [KEY1, KEY2] {
      Mock::given(method("POST"))
        .and(path("/go%20alert/sub/api/v2/generic/incoming"))
        .and(header("authorization", format!("Bearer {key}")))
        .and(body_json(body.clone()))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("goalert://{}/go%20alert/sub/{}/{KEY1}?dedup=disk&+env=prod", authority(&server), KEY2.to_uppercase())));
    assert!(a.notify(&ctx("", "50% done_now *", NotifyType::Success)).await.is_success());
  }

  #[tokio::test]
  async fn test_summary_from_a_passthrough_body_keeps_its_backslashes() {
    // Matches upstream's notify(body_format=None), then body_format="markdown"
    let body = r"Usage at **95%** on \_db01\_ <i> & more";
    for (passthrough, format, summary) in [(true, NotifyFormat::Text, body), (false, NotifyFormat::Markdown, r"Usage at **95%** on _db01_ <i> & more")] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/api/v2/generic/incoming"))
        .and(header("authorization", format!("Bearer {KEY1}")))
        .and(body_json(json!({"summary": summary, "details": body})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
      let mut a = crate::Apprise::new();
      assert!(a.add(&format!("goalert://{}/{KEY1}", authority(&server))));
      let c = NotifyContext { body_passthrough: passthrough, body_format: format, ..ctx("", body, NotifyType::Info) };
      assert!(a.notify(&c).await.is_success(), "passthrough={passthrough}");
    }
  }

  #[test]
  fn test_actions() {
    let action = |url: &str, notify_type| goalert(url).payload(&ctx("", "body", notify_type)).get("action").cloned();
    // Mapping closes alerts on success only
    let url = format!("goalert://localhost/{KEY1}");
    assert_eq!(action(&url, NotifyType::Success), Some(json!("close")));
    assert_eq!(action(&url, NotifyType::Info), None);
    assert_eq!(action(&url, NotifyType::Failure), None);
    // Explicit actions ignore the notification type
    assert_eq!(action(&format!("{url}?action=trigger"), NotifyType::Success), None);
    assert_eq!(action(&format!("{url}?action=c"), NotifyType::Failure), Some(json!("close")));
    // A long body is trimmed to the summary limit
    let p = goalert(&url).payload(&ctx("", &"a".repeat(3000), NotifyType::Info));
    assert_eq!(p["summary"].as_str().unwrap().len(), 1024);
    assert_eq!(p["details"].as_str().unwrap().len(), 3000);
  }

  #[tokio::test]
  async fn test_send_without_keys() {
    let svc = from_url("goalerts://localhost/?to=invalid").unwrap();
    assert!(!svc.send(&ctx("", "body", NotifyType::Info)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_partial_failure() {
    // One key fails, the other is still notified
    let server = MockServer::start().await;
    let route = "/api/v2/generic/incoming";
    Mock::given(method("POST"))
      .and(path(route))
      .and(header("authorization", format!("Bearer {KEY1}")))
      .respond_with(ResponseTemplate::new(401))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(route))
      .and(header("authorization", format!("Bearer {KEY2}")))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("goalert://{}/{KEY1}/{KEY2}", authority(&server))).unwrap();
    assert!(!svc.send(&ctx("", "body", NotifyType::Info)).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered() {
    // KEY2 always fails; KEY1 is never notified twice
    let server = MockServer::start().await;
    let route = "/api/v2/generic/incoming";
    Mock::given(method("POST"))
      .and(path(route))
      .and(header("authorization", format!("Bearer {KEY1}")))
      .respond_with(ResponseTemplate::new(200))
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(route))
      .and(header("authorization", format!("Bearer {KEY2}")))
      .respond_with(ResponseTemplate::new(500))
      .mount(&server)
      .await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("goalert://{}/{KEY1}/{KEY2}?retry=2&wait=0", authority(&server))));
    assert!(!a.notify(&ctx("", "body", NotifyType::Info)).await.is_success());
    let requests = server.received_requests().await.unwrap();
    let keys: Vec<String> = requests
      .iter()
      .filter(|r| r.url.path() == route)
      .filter_map(|r| r.headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from))
      .collect();
    assert_eq!(keys.iter().filter(|k| **k == format!("Bearer {KEY1}")).count(), 1);
    assert_eq!(keys.iter().filter(|k| **k == format!("Bearer {KEY2}")).count(), 3);
  }

  #[test]
  fn test_service_details() {
    let svc = from_url(&format!("goalert://localhost/{KEY1}")).unwrap();
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!(svc.title_maxlen(), 1024);
    assert_eq!(svc.body_maxlen(), 6144);
    assert_eq!(GoAlert::static_details().protocols, vec!["goalert", "goalerts"]);
  }
}
