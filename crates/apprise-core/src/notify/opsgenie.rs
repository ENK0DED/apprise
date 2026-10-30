//! Opsgenie, through the Alert API:
//!
//!   opsgenie://[{user}@]{apikey}/[@{user}][/#{team}][/*{schedule}][/^{escalation}]
//!
//! A target without a prefix is a user. Jira Service Management took over
//! Opsgenie's API, so `jira://` ([`super::jira`]) is this same plugin
//! pointed at Atlassian. Actions other than `new` work on the alerts an
//! earlier notification created, whose request IDs are kept in the
//! persistent store. See <https://docs.opsgenie.com/docs/alert-api>.

use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{is_uuid4, parse_list, qsd_bool};
use crate::utils::qsd::validate_token;
use async_trait::async_trait;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::time::Duration;

/// Upstream `OPSGENIE_ACTIONS`, in the order a partial `?action=` is matched.
const ACTIONS: &[&str] = &["map", "new", "close", "delete", "acknowledge", "note"];

/// Upstream `OPSGENIE_PRIORITY_MAP`, checked in order against the start of
/// the lower-cased `?priority=`.
const PRIORITY_MAP: &[(&str, u8)] =
  &[("l", 1), ("m", 2), ("n", 3), ("h", 4), ("e", 5), ("1", 1), ("2", 2), ("3", 3), ("4", 4), ("5", 5), ("p1", 1), ("p2", 2), ("p3", 3), ("p4", 4), ("p5", 5)];

/// The default (normal) priority.
const DEFAULT_PRIORITY: u8 = 3;

/// Targets per request in batch mode.
const BATCH_SIZE: usize = 50;

/// A longer message is cut down and the whole text left to the description.
const MESSAGE_MAXLEN: usize = 130;

/// How long the request IDs of created alerts are kept (60 days).
const REQUEST_ID_EXPIRY: Duration = Duration::from_secs(60 * 60 * 24 * 60);

const NOTIFY_TYPES: [NotifyType; 4] = [NotifyType::Info, NotifyType::Success, NotifyType::Warning, NotifyType::Failure];

/// Upstream `opsgenie_message_map`: the action taken for each of
/// [`NOTIFY_TYPES`] with the `map` action.
const DEFAULT_MAPPING: [&str; 4] = ["note", "close", "new", "new"];

/// The service an Opsgenie-style plugin talks to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Flavor {
  Opsgenie,
  /// Jira Service Management's port of the Opsgenie API.
  Jira,
}

impl Flavor {
  fn name(self) -> &'static str {
    match self {
      Self::Opsgenie => "Opsgenie",
      Self::Jira => "Jira",
    }
  }

  fn schema(self) -> &'static str {
    match self {
      Self::Opsgenie => "opsgenie",
      Self::Jira => "jira",
    }
  }

  /// The alert API of `region` (Jira keeps `?region=` for an easy move
  /// from Opsgenie but has one endpoint).
  fn alerts_url(self, region: &str) -> String {
    match (self, region) {
      (Self::Opsgenie, "eu") => format!("{}/v2/alerts", service_base("https://api.eu.opsgenie.com")),
      (Self::Opsgenie, _) => format!("{}/v2/alerts", service_base("https://api.opsgenie.com")),
      (Self::Jira, _) => format!("{}/jsm/ops/integration/v2/alerts", service_base("https://api.atlassian.com")),
    }
  }
}

/// Upstream `is_uuid()`: the entry starts with a version 4 UUID.
fn is_uuid(value: &str) -> bool {
  value.get(..36).is_some_and(is_uuid4)
}

/// One responder of an alert, from a target such as `#team` or `@user`.
fn responder(target: &str) -> Value {
  let (kind, name_field, name) = match target.chars().next() {
    Some('#') => ("team", "name", &target[1..]),
    Some('*') => ("schedule", "name", &target[1..]),
    Some('^') => ("escalation", "name", &target[1..]),
    Some('@') => ("user", "username", &target[1..]),
    // Ambiguous; treat it as a user
    _ => {
      tracing::debug!("Treating ambiguous target {} as a user", target);
      ("user", "username", target)
    }
  };
  let field = if is_uuid(name) { "id" } else { name_field };
  json!({ "type": kind, field: name })
}

pub struct Opsgenie {
  flavor: Flavor,
  apikey: String,
  user: Option<String>,
  targets: Vec<Value>,
  region: &'static str,
  details: Vec<(String, String)>,
  priority: u8,
  alias: Option<String>,
  entity: Option<String>,
  batch_size: usize,
  alert_tags: Vec<String>,
  action: &'static str,
  /// The action for each of [`NOTIFY_TYPES`] with the `map` action.
  mapping: [&'static str; 4],
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Opsgenie {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    Self::parse(url, Flavor::Opsgenie)
  }

  pub(crate) fn parse(url: &ParsedUrl, flavor: Flavor) -> Option<Self> {
    let name = flavor.name();
    let Some(apikey) = arg(url, "apikey").or_else(|| url.host.clone()).and_then(|k| validate_token(&k)) else {
      tracing::warn!("An invalid {} API Key was specified.", name);
      return None;
    };

    // The path holds our targets; ?to= adds more
    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.push(to.to_string());
    }
    let mut targets = Vec::new();
    for target in parse_list(entries, true, true) {
      let target = target.trim();
      if target.chars().count() < 2 {
        tracing::debug!("Ignoring {} Entry: {}", name, target);
        continue;
      }
      targets.push(responder(target));
    }

    let priority = arg(url, "priority")
      .map(|p| p.to_lowercase())
      .and_then(|p| PRIORITY_MAP.iter().find(|(k, _)| p.starts_with(k)).map(|(_, v)| *v))
      .unwrap_or(DEFAULT_PRIORITY);

    let region = match arg(url, "region") {
      Some(r) => match ["us", "eu"].into_iter().find(|v| *v == r.to_lowercase()) {
        Some(r) => r,
        None => {
          tracing::warn!("The {} region specified ({}) is invalid.", name, r);
          return None;
        }
      },
      None => "us",
    };

    // Upstream matches the action as given, so a partial name must be
    // lower case
    let action = match arg(url, "action") {
      Some(a) => match incident::match_prefix(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The {} action specified ({}) is invalid.", name, a);
          return None;
        }
      },
      None => "map",
    };

    let mut mapping = DEFAULT_MAPPING;
    for (key, value) in incident::unquoted_map(&url.qsd_colon) {
      let Some(index) = NOTIFY_TYPES.iter().position(|t| t.as_str().starts_with(&key)) else {
        tracing::warn!("The {} mapping key specified ({}) is invalid.", name, key);
        return None;
      };
      // Any action but map
      let Some(mapped) = incident::match_prefix(&ACTIONS[1..], &value.to_lowercase()) else {
        tracing::warn!("The {} mapping value (assigned to {}) specified ({}) is invalid.", name, NOTIFY_TYPES[index].as_str(), value);
        return None;
      };
      mapping[index] = mapped;
    }

    Some(Self {
      flavor,
      apikey,
      user: url.user.clone(),
      targets,
      region,
      details: incident::unquoted_map(&url.qsd_add),
      priority,
      alias: arg(url, "alias"),
      entity: arg(url, "entity"),
      batch_size: if qsd_bool(url, "batch", false) { BATCH_SIZE } else { 1 },
      alert_tags: arg(url, "tags").map(|t| parse_list([t], true, true)).unwrap_or_default(),
      action,
      mapping,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Opsgenie",
      service_url: Some("https://opsgenie.com/"),
      setup_url: Some("https://appriseit.com/services/opsgenie/"),
      protocols: vec!["opsgenie"],
      description: "Create, note, acknowledge, close and delete Opsgenie alerts.",
      attachment_support: false,
    }
  }

  /// The action this notification takes.
  fn resolve_action(&self, notify_type: &NotifyType) -> &'static str {
    if self.action == "map" { self.mapping[NOTIFY_TYPES.iter().position(|t| t == notify_type).unwrap_or(0)] } else { self.action }
  }

  /// The persistent store key of the alerts this notification is about.
  fn store_key(&self, ctx: &NotifyContext) -> String {
    let source = self.entity.as_deref().or(self.alias.as_deref()).unwrap_or(if ctx.title.is_empty() { ctx.asset.app_id.as_str() } else { ctx.title.as_str() });
    hex::encode(Sha1::digest(source.as_bytes()))[..10].to_string()
  }

  /// The payload of a new alert, without its responders.
  fn new_payload(&self, ctx: &NotifyContext) -> Value {
    let mut details = incident::json_object(&self.details);
    if details.get("type").is_none() {
      details["type"] = json!(ctx.notify_type.as_str());
    }
    // Use the body when there is no title, cut down when it is long
    let title_body = if ctx.title.is_empty() { &ctx.body } else { &ctx.title };
    let message = if title_body.chars().count() > MESSAGE_MAXLEN {
      format!("{}...", title_body.chars().take(MESSAGE_MAXLEN - 3).collect::<String>())
    } else {
      title_body.clone()
    };
    let mut payload = json!({
      "source": ctx.asset.app_desc,
      "message": message,
      "description": ctx.body,
      "details": details,
      "priority": format!("P{}", self.priority),
    });
    if !self.alert_tags.is_empty() {
      payload["tags"] = json!(self.alert_tags);
    }
    if let Some(entity) = &self.entity {
      payload["entity"] = json!(entity);
    }
    if let Some(alias) = &self.alias {
      payload["alias"] = json!(alias);
    }
    if let Some(user) = &self.user {
      payload["user"] = json!(user);
    }
    payload
  }

  /// Send one request; returns whether it was accepted and the request ID
  /// in the reply.
  async fn fetch(&self, request: reqwest::RequestBuilder, payload: &Value) -> (bool, Option<String>) {
    let mut request = request.header("User-Agent", APP_ID).header("Authorization", format!("GenieKey {}", self.apikey));
    if self.flavor == Flavor::Jira {
      // Upstream's header name, kept as it is
      request = request.header("Accepts", "application/json");
    }
    let what = format!("{} notification", self.flavor.name());
    match incident::exchange(request.json(payload), &what, &[200, 202], &[]).await {
      Ok(reply) => {
        let content: Value = serde_json::from_str(&reply.body).unwrap_or_else(|_| {
          tracing::debug!("Failed to parse {} JSON response.", self.flavor.name());
          json!({})
        });
        (reply.delivered, content.get("requestId").and_then(Value::as_str).map(str::to_string))
      }
      Err(e) => {
        incident::log_connection_error(&what, &e);
        (false, None)
      }
    }
  }

  pub(crate) async fn send_alerts(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let action = self.resolve_action(&ctx.notify_type);
    let url = self.flavor.alerts_url(self.region);
    let client = build_client(self.verify_certificate)?;
    let key = self.store_key(ctx);

    // The request IDs of the alerts created earlier; without a store
    // nothing was kept
    let stored = crate::storage::with_service_store(|s| s.get(&key)).flatten();
    let mut request_ids: Vec<String> = match stored {
      Some(Value::Array(ids)) => ids.iter().filter_map(|id| id.as_str().map(str::to_string)).collect(),
      _ => Vec::new(),
    };

    let mut has_error = false;
    if action == "new" {
      let mut payload = self.new_payload(ctx);
      let batches: Vec<usize> = (0..self.targets.len().max(1)).step_by(self.batch_size).collect();
      // Start a new ID list unless an earlier attempt completed a batch
      if !batches.iter().any(|i| crate::delivery::is_delivered(&i.to_string(), false)) {
        request_ids.clear();
      }
      for index in batches {
        // Skip a batch that already accepted this message so a retry does
        // not deliver it twice
        if crate::delivery::is_delivered(&index.to_string(), false) {
          continue;
        }
        if !self.targets.is_empty() {
          payload["responders"] = json!(self.targets[index..(index + self.batch_size).min(self.targets.len())]);
        }
        match self.fetch(client.post(&url), &payload).await {
          (true, Some(id)) => request_ids.push(id),
          _ => {
            has_error = true;
            continue;
          }
        }
        crate::delivery::mark_delivered(&index.to_string(), false);
      }
      crate::storage::with_service_store(|s| s.set(&key, json!(request_ids), Some(REQUEST_ID_EXPIRY), true));
    } else if !request_ids.is_empty() {
      let mut payload = json!({ "source": ctx.asset.app_desc, "note": ctx.body });
      if let Some(user) = &self.user {
        payload["user"] = json!(user);
      }
      for id in &request_ids {
        let request = match action {
          "delete" => client.delete(format!("{}/{}", url, id)),
          "acknowledge" => client.post(format!("{}/{}/acknowledge", url, id)),
          "close" => client.post(format!("{}/{}/close", url, id)),
          _ => client.post(format!("{}/{}/notes", url, id)),
        };
        if !self.fetch(request.query(&[("identifierType", "id")]), &payload).await.0 {
          has_error = true;
        }
      }
      if !has_error && action == "delete" {
        crate::storage::with_service_store(|s| s.clear(&[key.as_str()]));
      }
    } else {
      tracing::info!("No {} notification sent due to (nothing to {}) condition", self.flavor.name(), self.action);
    }
    Ok(!has_error)
  }

  pub(crate) fn identifier(&self) -> Vec<String> {
    vec![self.flavor.schema().into(), self.region.into(), self.apikey.clone()]
  }
}

#[async_trait]
impl Notify for Opsgenie {
  fn schemas(&self) -> &[&str] {
    &["opsgenie"]
  }
  fn service_name(&self) -> &str {
    "Opsgenie"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    15000
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(self.identifier())
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    self.send_alerts(ctx).await
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  pub(crate) const UUID4: &str = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";
  pub(crate) const GOOD_RESPONSE: &str = r#"{"result": "Request will be processed", "took": 0.204, "requestId": "43a29c5c-3dbf-4fa4-9c26-f4f71023e120"}"#;

  fn opsgenie(url: &str) -> Opsgenie {
    Opsgenie::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  pub(crate) fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  /// Upstream's URL test table, for `opsgenie://` and `jira://`.
  pub(crate) fn valid_urls(schema: &str) -> Vec<String> {
    [
      "user@apikey/",
      "apikey/",
      "apikey/user",
      "apikey/@user?region=eu",
      "apikey/@user?entity=A%20Entity",
      "apikey/@user?alias=An%20Alias",
      "apikey/@user?entity=index&action=new",
      "apikey/@user?entity=index&action=acknowledge",
      "from@apikey/@user?entity=index&action=note",
      "apikey/@user?entity=index&action=close",
      "apikey/@user?entity=index&action=delete",
      "apikey/@user?entity=index2&:info=new",
      "joe@apikey/@user?priority=p3",
      "apikey/?tags=comma,separated",
      "apikey/@user?priority=invalid",
      "apikey/user@email.com/#team/*sche/^esc/%20/a",
      "apikey/#team/@user/?batch=yes",
      "apikey/#team/@user/?batch=no",
      "?apikey=abc&to=user",
      "apikey/#team/user/",
      "apikey/#topic1/device/",
    ]
    .iter()
    .map(|u| format!("{schema}://{u}"))
    .chain([format!("{schema}://apikey/@{UUID4}/#{UUID4}/*{UUID4}/^{UUID4}/"), format!("{schema}://apikey/{UUID4}/#{UUID4}/*{UUID4}/^{UUID4}/")])
    .chain([format!("{schema}://apikey?to=#team,user&+key=value&+type=override")])
    .collect()
  }

  pub(crate) fn invalid_urls(schema: &str) -> Vec<String> {
    ["", ":@/", "%20%20/", "apikey/user/?region=xx", "apikey/@user?action=invalid", "from@apikey/@user?:invalid=note", "apikey/@user?:warning=invalid"]
      .iter()
      .map(|u| format!("{schema}://{u}"))
      .collect()
  }

  #[test]
  fn test_valid_urls() {
    for url in valid_urls("opsgenie") {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in invalid_urls("opsgenie") {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = opsgenie("opsgenie://joe@apikey/user@email.com/#team/*sche/^esc/%20/a?region=EU&entity=E&alias=A&tags=b,a&batch=yes&+key=value");
    assert_eq!(obj.apikey, "apikey");
    assert_eq!(obj.user.as_deref(), Some("joe"));
    assert_eq!(obj.region, "eu");
    assert_eq!(obj.batch_size, BATCH_SIZE);
    assert_eq!(obj.alert_tags, vec!["a", "b"]);
    assert_eq!((obj.entity.as_deref(), obj.alias.as_deref()), (Some("E"), Some("A")));
    // Sorted; too short entries are dropped
    assert_eq!(
      obj.targets,
      vec![
        json!({"type": "team", "name": "team"}),
        json!({"type": "schedule", "name": "sche"}),
        json!({"type": "escalation", "name": "esc"}),
        json!({"type": "user", "username": "user@email.com"}),
      ]
    );
    // UUIDs are sent as IDs
    let obj = opsgenie(&format!("opsgenie://apikey/@{UUID4}/#{UUID4}/*{UUID4}/^{UUID4}/{UUID4}"));
    assert!(obj.targets.iter().all(|t| t["id"] == UUID4));
    // ?to= adds targets
    let obj = opsgenie("opsgenie://?apikey=abc&to=%23team,user");
    assert_eq!(obj.apikey, "abc");
    assert_eq!(obj.targets, vec![json!({"type": "team", "name": "team"}), json!({"type": "user", "username": "user"})]);
    assert_eq!(obj.batch_size, 1);
  }

  #[test]
  fn test_priorities() {
    for (value, expected) in
      [("1", 1), ("low", 1), ("p1", 1), ("moderate", 2), ("normal", 3), ("p3", 3), ("HIGH", 4), ("5", 5), ("emergency", 5), ("p5", 5), ("invalid", 3), ("", 3)]
    {
      assert_eq!(opsgenie(&format!("opsgenie://apikey/user?priority={value}")).priority, expected, "priority={}", value);
    }
  }

  #[test]
  fn test_actions_and_mapping() {
    let obj = opsgenie("opsgenie://apikey");
    assert_eq!(obj.action, "map");
    let actions: Vec<&str> = NOTIFY_TYPES.iter().map(|t| obj.resolve_action(t)).collect();
    assert_eq!(actions, ["note", "close", "new", "new"]);
    assert_eq!(opsgenie("opsgenie://apikey?action=a").action, "acknowledge");
    assert_eq!(opsgenie("opsgenie://apikey?action=n").action, "new");
    assert_eq!(opsgenie("opsgenie://apikey?action=ack").resolve_action(&NotifyType::Info), "acknowledge");
    // Mappings replace the default for their type
    let obj = opsgenie("opsgenie://apikey?:info=new&:s=DEL&:w=a");
    let actions: Vec<&str> = NOTIFY_TYPES.iter().map(|t| obj.resolve_action(t)).collect();
    assert_eq!(actions, ["new", "delete", "acknowledge", "new"]);
  }

  #[tokio::test]
  async fn test_send_new() {
    // Matches pycapture.py for this URL: one request per target, sorted
    let server = MockServer::start().await;
    for responder in [json!({"type": "team", "name": "team"}), json!({"type": "user", "username": "user"})] {
      Mock::given(method("POST"))
        .and(path("/v2/alerts"))
        .and(header("authorization", "GenieKey apikey"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
          "source": "Apprise Notifications",
          "message": "T",
          "description": "B",
          "details": {"type": "failure"},
          "priority": "P3",
          "responders": [responder],
        })))
        .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"requestId": "abc"}"#))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("opsgenie://apikey/@user/#team").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Failure))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_new_options() {
    // Matches pycapture.py: a long title is cut down, ?to= targets and a
    // +type detail that overrides the notification type
    let server = MockServer::start().await;
    let message = format!("{}...", "x".repeat(127));
    Mock::given(method("POST"))
      .and(path("/v2/alerts"))
      .and(body_json(json!({
        "source": "Apprise Notifications",
        "message": message,
        "description": "B",
        "details": {"type": "override"},
        "priority": "P3",
        "responders": [{"type": "team", "name": "team"}],
      })))
      .respond_with(ResponseTemplate::new(202).set_body_string(GOOD_RESPONSE))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("opsgenie://apikey?to=%23team&+type=override").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx(&"x".repeat(140), "B", NotifyType::Failure))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    // Batch mode sends every responder at once, in the EU region
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/alerts"))
      .and(body_json(json!({
        "source": "Apprise Notifications",
        "message": "T",
        "description": "B",
        "details": {"key": "value", "type": "warning"},
        "priority": "P4",
        "tags": ["a", "b"],
        "entity": "E",
        "alias": "A",
        "user": "joe",
        "responders": [
          {"type": "team", "name": "team"},
          {"type": "schedule", "name": "sched"},
          {"type": "user", "username": "user"},
          {"type": "escalation", "name": "esc"},
        ],
      })))
      .respond_with(ResponseTemplate::new(202).set_body_string(GOOD_RESPONSE))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("opsgenie://joe@apikey/@user/%23team/*sched/^esc?priority=high&tags=b,a&entity=E&alias=A&+key=value&batch=yes&region=eu").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Warning))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_bad_response() {
    // An accepted request without a request ID is a failure
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v2/alerts")).respond_with(ResponseTemplate::new(202).set_body_string("{")).expect(1).mount(&server).await;
    let svc = from_url("opsgenie://user@apikey/").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Warning))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_nothing_to_act_on() {
    // Without stored alerts, a note (the default for info) sends nothing
    let server = MockServer::start().await;
    let svc = from_url("opsgenie://apikey/@user").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Info))).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|r| r.url.path().starts_with("/v2/alerts")));
  }

  /// Create an alert, then act on it through the persistent store.
  pub(crate) async fn store_round_trip(schema: &str, alerts_path: &str) {
    let id = "43a29c5c-3dbf-4fa4-9c26-f4f71023e120";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(alerts_path)).respond_with(ResponseTemplate::new(202).set_body_string(GOOD_RESPONSE)).expect(1).mount(&server).await;
    for (verb, suffix) in [("POST", "/acknowledge"), ("POST", "/notes"), ("POST", "/close"), ("DELETE", "")] {
      Mock::given(method(verb))
        .and(path(format!("{alerts_path}/{id}{suffix}")))
        .and(query_param("identifierType", "id"))
        .and(body_json(json!({"source": "Apprise Notifications", "note": "B", "user": "from"})))
        .respond_with(ResponseTemplate::new(202).set_body_string(GOOD_RESPONSE))
        .expect(1)
        .mount(&server)
        .await;
    }

    let tmp = tempfile::tempdir().unwrap();
    let asset = crate::AppriseAsset { storage_path: Some(tmp.path().to_path_buf()), ..Default::default() };
    let mut a = crate::Apprise::with_asset(asset);
    for action in ["new", "acknowledge", "note", "close", "delete", "close"] {
      a.clear();
      assert!(a.add(&format!("{schema}://from@apikey/@user?entity=index&action={action}")));
      let result = crate::notify::with_test_base(server.uri(), a.notify(&ctx("T", "B", NotifyType::Failure))).await;
      assert!(result.is_success(), "{}", action);
    }
    // The delete forgot the alert, so the last close sent nothing
  }

  #[tokio::test]
  async fn test_store_round_trip() {
    store_round_trip("opsgenie", "/v2/alerts").await;
  }

  #[tokio::test]
  async fn test_store_keys() {
    // The same title (or entity, or alias) shares one store entry
    let obj = opsgenie("opsgenie://apikey");
    assert_eq!(obj.store_key(&ctx("key", "test", NotifyType::Failure)), "a62f2225bf");
    assert_eq!(obj.store_key(&ctx("", "test", NotifyType::Failure)), hex::encode(Sha1::digest(b"Apprise"))[..10]);
    assert_eq!(opsgenie("opsgenie://apikey?entity=e&alias=a").store_key(&ctx("t", "b", NotifyType::Info)), hex::encode(Sha1::digest(b"e"))[..10]);
    assert_eq!(opsgenie("opsgenie://apikey?alias=a").store_key(&ctx("t", "b", NotifyType::Info)), hex::encode(Sha1::digest(b"a"))[..10]);
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/v2/alerts"))
        .respond_with(ResponseTemplate::new(status).set_body_string(GOOD_RESPONSE))
        .expect(2)
        .mount(&server)
        .await;
      let svc = from_url("opsgenie://apikey/#team/user/").unwrap();
      // Both targets are tried
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Failure))).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let svc = from_url("opsgenie://apikey").unwrap();
    assert_eq!(svc.service_name(), "Opsgenie");
    assert_eq!(svc.body_maxlen(), 15000);
    assert_eq!(Opsgenie::static_details().protocols, vec!["opsgenie"]);
  }
}
