//! Splunk On-Call (formerly VictorOps), through the REST endpoint
//! integration:
//!
//!   splunk://{routing_key}@{apikey}[/{entity_id}]
//!
//! `victorops://` is accepted too, as is the endpoint URL itself
//! (`https://alert.victorops.com/integrations/generic/20131114/alert/...`).
//! See <https://portal.victorops.com/public/api-docs.html>.

use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `SPLUNK_ACTIONS`, in the order a partial `?action=` is matched.
const ACTIONS: &[&str] = &["map", "info", "acknowledgement", "warning", "recovery", "resolve", "critical"];

/// Upstream `SPLUNK_MESSAGE_TYPES`, in the order a `:type=` value is matched.
const MESSAGE_TYPES: &[&str] = &["CRITICAL", "WARNING", "ACKNOWLEDGEMENT", "INFO", "RECOVERY"];

/// Upstream `splunk_message_map`: the message type sent for each
/// notification type with the `map` action.
fn default_message_type(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    // Creates a timeline event but does not trigger an incident
    NotifyType::Info => "INFO",
    // Resolves an incident
    NotifyType::Success => "RECOVERY",
    // May trigger an incident, depending on your settings
    NotifyType::Warning => "WARNING",
    // Triggers an incident
    NotifyType::Failure => "CRITICAL",
  }
}

const NOTIFY_TYPES: [NotifyType; 4] = [NotifyType::Info, NotifyType::Success, NotifyType::Warning, NotifyType::Failure];

pub struct Splunk {
  apikey: String,
  routing_key: String,
  /// `None` uses `{app_id}/{routing_key}`.
  entity_id: Option<String>,
  action: &'static str,
  /// The message type for each of [`NOTIFY_TYPES`] with the `map` action.
  mapping: [&'static str; 4],
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Splunk {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = arg(url, "apikey").or_else(|| url.host.clone()).unwrap_or_default();
    if !incident::is_key(&apikey) {
      tracing::warn!("The Splunk API Key specified is invalid.");
      return None;
    }
    let routing_key = arg(url, "routing_key").or_else(|| arg(url, "route")).or_else(|| url.user.clone()).unwrap_or_default();
    if !incident::is_key(&routing_key) {
      tracing::warn!("The Splunk Routing Key specified ({}) is invalid.", routing_key);
      return None;
    }

    // Such as "disk space/db01.mycompany.com"; the whole path when not
    // given as an argument
    let entity_id = arg(url, "entity_id").unwrap_or_else(|| unquote(&url.fullpath()));
    let entity_id = entity_id.trim_matches([' ', '\r', '\n', '\t', '\x0b', '/']);
    let entity_id = (!entity_id.is_empty()).then(|| entity_id.to_string());

    // Upstream matches the action as given, so a partial name must be
    // lower case
    let action = match arg(url, "action") {
      Some(a) => match incident::match_prefix(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The Splunk action specified ({}) is invalid.", a);
          return None;
        }
      },
      None => "map",
    };

    let mut mapping = NOTIFY_TYPES.map(|t| default_message_type(&t));
    for (key, value) in incident::unquoted_map(&url.qsd_colon) {
      let Some(index) = NOTIFY_TYPES.iter().position(|t| t.as_str().starts_with(&key)) else {
        tracing::warn!("The Splunk mapping key specified ({}) is invalid.", key);
        return None;
      };
      let Some(message_type) = incident::match_prefix(MESSAGE_TYPES, &value.to_uppercase()) else {
        tracing::warn!("The Splunk mapping value (assigned to {}) specified ({}) is invalid.", NOTIFY_TYPES[index].as_str(), value);
        return None;
      };
      mapping[index] = message_type;
    }

    Some(Self { apikey, routing_key, entity_id, action, mapping, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Splunk On-Call",
      service_url: Some("https://www.splunk.com/en_us/products/on-call.html"),
      setup_url: Some("https://appriseit.com/services/splunk/"),
      protocols: vec!["splunk", "victorops"],
      description: "Raise, acknowledge and recover Splunk On-Call (VictorOps) incidents.",
      attachment_support: false,
    }
  }

  fn message_type(&self, notify_type: &NotifyType) -> &'static str {
    match self.action {
      "map" => self.mapping[NOTIFY_TYPES.iter().position(|t| t == notify_type).unwrap_or(0)],
      "acknowledgement" => "ACKNOWLEDGEMENT",
      "info" => "INFO",
      "critical" => "CRITICAL",
      "warning" => "WARNING",
      // recovery and resolve
      _ => "RECOVERY",
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    json!({
      "entity_id": self.entity_id.clone().unwrap_or_else(|| format!("{}/{}", ctx.asset.app_id, self.routing_key)),
      "message_type": self.message_type(&ctx.notify_type),
      "entity_display_name": if ctx.title.is_empty() { ctx.asset.app_desc.as_str() } else { ctx.title.as_str() },
      "state_message": ctx.body,
      "monitoring_tool": ctx.asset.app_id,
    })
  }
}

#[async_trait]
impl Notify for Splunk {
  fn schemas(&self) -> &[&str] {
    &["splunk", "victorops"]
  }
  fn service_name(&self) -> &str {
    "Splunk On-Call"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    60
  }
  fn body_maxlen(&self) -> usize {
    400
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["splunk".into(), self.routing_key.clone(), self.entity_id.clone().unwrap_or_default(), self.apikey.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/integrations/generic/20131114/alert/{}/{}", service_base("https://alert.victorops.com"), self.apikey, self.routing_key);
    let request = client.post(url).header("User-Agent", APP_ID).json(&self.payload(ctx));
    incident::exchange(request, "Splunk notification", &[200], &[]).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn splunk(url: &str) -> Splunk {
    Splunk::from_url(&crate::notify::registry::parse_service_url(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "splunk://?apikey=abc123&routing_key=db",
      "splunk://route@abc123/entity_id",
      "splunk://route@abc123/?entity_id=my_entity",
      "splunk://route@abc123/?entity_id=a%3Fb%23c%25d%2Fe",
      "https://alert.victorops.com/integrations/generic/20131114/alert/apikey/routing_key",
      "https://alert.victorops.com/integrations/generic/20131114/alert/apikey/routing_key/entity_id",
      "victorops://?apikey=abc123&route=db",
      "splunk://?apikey=abc123&route=db",
      "splunk://db@apikey?action=recovery",
      "splunk://db@apikey?action=resolve",
      "splunk://db@apikey?action=r",
      "splunk://db@apikey?action=acknowledgement",
      "splunk://db@apikey?action=ack",
      "splunk://db@apikey?action=critical",
      "splunk://db@apikey?action=crit",
      "splunk://db@apikey?action=warning",
      "splunk://db@apikey?action=warn",
      "splunk://db@apikey?action=info",
      "splunk://db@apikey?action=i",
      "splunk://db@apikey?:warning=critical",
      "splunk://db@token",
    ];
    for url in urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "splunk://",
      "splunk://:@/",
      "splunk://routekey@%badapi%",
      "splunk://abc123",
      "splunk://%badroute%@apikey",
      "splunk://db@apikey?action=invalid",
      "splunk://db@apikey?:invalid=critical",
      "splunk://db@apikey?:warning=invalid",
      // Matched as given, as upstream does
      "splunk://db@apikey?action=ACK",
      "splunk://db@apikey?:Warning=critical",
    ];
    for url in urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = splunk("splunk://route@abc123");
    assert_eq!((obj.apikey.as_str(), obj.routing_key.as_str()), ("abc123", "route"));
    assert!(obj.entity_id.is_none());
    assert_eq!(obj.action, "map");
    // The whole path is the entity ID, without surrounding slashes
    assert_eq!(splunk("splunk://route@abc123/disk%20space/db01.example.com/").entity_id.as_deref(), Some("disk space/db01.example.com"));
    assert!(splunk("splunk://route@abc123/%20%2F").entity_id.is_none());
    // Reserved characters survive an encoded ?entity_id=
    assert_eq!(splunk("splunk://route@abc123/?entity_id=a%3Fb%23c%25d%2Fe").entity_id.as_deref(), Some("a?b#c%d/e"));
    // ?routing_key= wins over ?route=, which wins over the user
    assert_eq!(splunk("splunk://u@abc123?route=b&routing_key=a").routing_key, "a");
    assert_eq!(splunk("splunk://u@abc123?route=b").routing_key, "b");
    assert_eq!(splunk("splunk://db@apikey?action=r").action, "recovery");
    assert_eq!(splunk("splunk://db@apikey?action=ack").action, "acknowledgement");
    assert_eq!(splunk("splunk://db@apikey?:warning=crit&:s=info").mapping, ["INFO", "INFO", "CRITICAL", "CRITICAL"]);
    // The native URL keeps its entity ID
    let obj = splunk("https://alert.victorops.com/integrations/generic/20131114/alert/apikey/routing_key/entity/id");
    assert_eq!((obj.apikey.as_str(), obj.routing_key.as_str(), obj.entity_id.as_deref()), ("apikey", "routing_key", Some("entity/id")));
  }

  #[tokio::test]
  async fn test_send_map() {
    // Matches pycapture.py for each notification type
    for (notify_type, message_type) in
      [(NotifyType::Info, "INFO"), (NotifyType::Success, "RECOVERY"), (NotifyType::Warning, "WARNING"), (NotifyType::Failure, "CRITICAL")]
    {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/integrations/generic/20131114/alert/abc123/route"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
          "entity_id": "Apprise/route",
          "message_type": message_type,
          "entity_display_name": "T",
          "state_message": "B",
          "monitoring_tool": "Apprise",
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url("splunk://route@abc123").unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", notify_type))).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_send_entity_and_action() {
    // Matches pycapture.py: no title uses the asset description
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/integrations/generic/20131114/alert/abc123/route"))
      .and(body_json(json!({
        "entity_id": "a?b#c%d/e",
        "message_type": "ACKNOWLEDGEMENT",
        "entity_display_name": "Apprise Notifications",
        "state_message": "B",
        "monitoring_tool": "Apprise",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("splunk://route@abc123/?entity_id=a%3Fb%23c%25d%2Fe&action=ack").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "B", NotifyType::Info))).await.unwrap());
  }

  #[test]
  fn test_actions() {
    let message_type = |url: &str, notify_type| splunk(url).message_type(&notify_type);
    for notify_type in [NotifyType::Info, NotifyType::Success, NotifyType::Warning, NotifyType::Failure] {
      assert_eq!(message_type("splunk://db@apikey?action=resolve", notify_type.clone()), "RECOVERY");
      assert_eq!(message_type("splunk://db@apikey?action=recovery", notify_type.clone()), "RECOVERY");
      assert_eq!(message_type("splunk://db@apikey?action=ack", notify_type.clone()), "ACKNOWLEDGEMENT");
      assert_eq!(message_type("splunk://db@apikey?action=crit", notify_type.clone()), "CRITICAL");
      assert_eq!(message_type("splunk://db@apikey?action=warn", notify_type.clone()), "WARNING");
      assert_eq!(message_type("splunk://db@apikey?action=i", notify_type), "INFO");
    }
    // A mapping replaces the default for its type
    assert_eq!(message_type("splunk://db@apikey?:warning=critical", NotifyType::Warning), "CRITICAL");
    assert_eq!(message_type("splunk://db@apikey?:warning=critical", NotifyType::Success), "RECOVERY");
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [201, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/integrations/generic/20131114/alert/apikey/db"))
        .respond_with(ResponseTemplate::new(status))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url("splunk://db@apikey").unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b", NotifyType::Info))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }

  #[test]
  fn test_service_details() {
    let svc = from_url("splunk://db@apikey").unwrap();
    assert_eq!(svc.service_name(), "Splunk On-Call");
    assert_eq!(svc.title_maxlen(), 60);
    assert_eq!(svc.body_maxlen(), 400);
    assert_eq!(Splunk::static_details().protocols, vec!["splunk", "victorops"]);
  }
}
