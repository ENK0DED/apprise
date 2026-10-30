use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::parse_list;
use crate::utils::qsd::validate_token;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `PAGERTREE_ACTIONS`; anything else falls back to `create`.
const ACTIONS: &[&str] = &["create", "acknowledge", "resolve", "map"];

/// Upstream `PAGERTREE_URGENCIES`; anything else leaves the urgency unset.
const URGENCIES: &[&str] = &["silent", "low", "medium", "high", "critical"];

/// Upstream `^int_[a-zA-Z0-9\-_]{7,14}$` (case-insensitive).
fn is_integration_id(id: &str) -> bool {
  let Some(rest) = id.get(..4).filter(|p| p.eq_ignore_ascii_case("int_")).map(|_| &id[4..]) else { return false };
  (7..=14).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub struct PagerTree {
  integration: String,
  action: &'static str,
  thirdparty: Option<String>,
  urgency: Option<&'static str>,
  alert_tags: Vec<String>,
  headers: Vec<(String, String)>,
  payload_extras: Vec<(String, String)>,
  meta_extras: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl PagerTree {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let integration = arg(url, "id").or_else(|| arg(url, "integration")).or_else(|| url.host.clone()).unwrap_or_default();
    if !is_integration_id(&integration) {
      tracing::warn!("An invalid PagerTree Integration ID ({}) was specified.", integration);
      return None;
    }

    // An ID to acknowledge or resolve; ?dedup= is the name the other
    // on-call plugins use for it
    let thirdparty = match arg(url, "tid").or_else(|| arg(url, "thirdparty")).or_else(|| arg(url, "dedup")) {
      Some(id) => match validate_token(&id) {
        Some(id) => Some(id),
        None => {
          tracing::warn!("An invalid PagerTree third party ID ({}) was specified.", id);
          return None;
        }
      },
      None => None,
    };

    let action = arg(url, "action").and_then(|a| ACTIONS.iter().copied().find(|v| *v == a)).unwrap_or("create");
    let urgency = arg(url, "urgency").and_then(|u| URGENCIES.iter().copied().find(|v| *v == u));
    let alert_tags = arg(url, "tags").map(|t| parse_list([t], true, true)).unwrap_or_default();

    Some(Self {
      integration,
      action,
      thirdparty,
      urgency,
      alert_tags,
      headers: incident::unquoted_map(&url.qsd_add),
      payload_extras: incident::unquoted_map(&url.qsd_colon),
      meta_extras: incident::unquoted_map(&url.qsd_del),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "PagerTree",
      service_url: Some("https://pagertree.com/"),
      setup_url: Some("https://appriseit.com/services/pagertree/"),
      protocols: vec!["pagertree"],
      description: "Create, acknowledge and resolve PagerTree alerts.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Map resolves on success and creates an alert otherwise
    let event_type = match self.action {
      "map" if ctx.notify_type == NotifyType::Success => "resolve",
      "map" => "create",
      action => action,
    };

    // Our own ID always wins. Each created alert gets its own ID, kept
    // across retries so a retry never creates a second one. Every other
    // action needs an ID that stays the same between notifications, which
    // the title (or the first line of the body) stands in for.
    let alert_id = match &self.thirdparty {
      Some(id) => id.clone(),
      None if self.action == "create" => incident::retry_key("alert_id"),
      None => incident::title_key(&ctx.title, &ctx.body),
    };

    let mut payload = json!({ "id": alert_id, "event_type": event_type });
    if event_type == "create" {
      payload["title"] = json!(if ctx.title.is_empty() { ctx.asset.app_desc.as_str() } else { ctx.title.as_str() });
      payload["description"] = json!(ctx.body);
      payload["meta"] = incident::json_object(&self.meta_extras);
      payload["tags"] = json!(self.alert_tags);
      if let Some(urgency) = self.urgency {
        payload["urgency"] = json!(urgency);
      }
    }
    for (key, value) in &self.payload_extras {
      payload[key] = json!(value);
    }
    payload
  }
}

#[async_trait]
impl Notify for PagerTree {
  fn schemas(&self) -> &[&str] {
    &["pagertree"]
  }
  fn service_name(&self) -> &str {
    "PagerTree"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["pagertree".into(), self.integration.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let mut request = client.post(format!("{}/integration/{}", service_base("https://api.pagertree.com"), self.integration)).header("User-Agent", APP_ID);
    // Extra headers, such as a PagerTree token
    for (name, value) in &self.headers {
      request = request.header(name.as_str(), value.as_str());
    }
    incident::exchange(request.json(&self.payload(ctx)), "PagerTree notification", &[200, 201, 202], &[]).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const INTEGRATION_ID: &str = "int_xxxxxxxxxxx";

  fn pagertree(url: &str) -> PagerTree {
    PagerTree::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("pagertree://{INTEGRATION_ID}"),
      format!("pagertree://{INTEGRATION_ID}?integration=int_yyyyyyyyyy"),
      format!("pagertree://{INTEGRATION_ID}?id=int_zzzzzzzzzz"),
      format!("pagertree://{INTEGRATION_ID}?urgency=low"),
      format!("pagertree://?id={INTEGRATION_ID}&urgency=low"),
      format!("pagertree://{INTEGRATION_ID}?tags=production,web"),
      format!("pagertree://{INTEGRATION_ID}?action=resolve&thirdparty=123"),
      format!("pagertree://{INTEGRATION_ID}?+pagertree-token=123&:env=prod&-m=v"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "pagertree://".to_string(),
      format!("pagertree://{}", "+".repeat(24)),
      "pagertree://:@/".to_string(),
      // Not an int_ ID, or too short/long
      "pagertree://abc".to_string(),
      "pagertree://int_abc".to_string(),
      format!("pagertree://int_{}", "a".repeat(15)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = pagertree(&format!("pagertree://{INTEGRATION_ID}"));
    assert_eq!(obj.integration, INTEGRATION_ID);
    assert_eq!(obj.action, "create");
    assert!(obj.thirdparty.is_none() && obj.urgency.is_none());
    // A blank ID is trimmed away, as upstream does
    assert!(pagertree(&format!("pagertree://{INTEGRATION_ID}?tid=%20%20")).thirdparty.is_none());
    // ?id= wins over ?integration=, which wins over the host
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?id=int_zzzzzzzzzz&integration=int_yyyyyyyyyy")).integration, "int_zzzzzzzzzz");
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?integration=int_yyyyyyyyyy")).integration, "int_yyyyyyyyyy");
    // ?tid= wins over ?thirdparty=, which wins over ?dedup=
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?thirdparty=b&tid=a&dedup=c")).thirdparty.as_deref(), Some("a"));
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?thirdparty=b&dedup=c")).thirdparty.as_deref(), Some("b"));
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?action=map&dedup=disk-full")).thirdparty.as_deref(), Some("disk-full"));
    // Actions and urgencies must match exactly
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?action=map")).action, "map");
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?action=ack")).action, "create");
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?urgency=high")).urgency, Some("high"));
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?urgency=urgent")).urgency, None);
    // Alert tags are sorted
    assert_eq!(pagertree(&format!("pagertree://{INTEGRATION_ID}?tags=web,production")).alert_tags, vec!["production", "web"]);
  }

  #[tokio::test]
  async fn test_send_create() {
    // Matches pycapture.py for this URL (the ID is random)
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/integration/{INTEGRATION_ID}")))
      .and(header("pagertree-token", "123"))
      .and(header("content-type", "application/json"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("pagertree://{INTEGRATION_ID}?urgency=low&tags=web,production&+pagertree-token=123&:env=prod&-m=v")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Info))).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    let request = requests.iter().find(|r| r.url.path() == format!("/integration/{INTEGRATION_ID}")).unwrap();
    let mut sent: Value = serde_json::from_slice(&request.body).unwrap();
    assert!(crate::utils::plugin_args::is_uuid4(sent["id"].as_str().unwrap()));
    sent["id"] = json!("ID");
    assert_eq!(
      sent,
      json!({
        "id": "ID",
        "event_type": "create",
        "title": "T",
        "description": "B",
        "meta": {"m": "v"},
        "tags": ["production", "web"],
        "urgency": "low",
        "env": "prod",
      })
    );
  }

  #[tokio::test]
  async fn test_send_map_resolve() {
    // Matches pycapture.py: only the ID and event type are sent
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/integration/{INTEGRATION_ID}")))
      .and(body_json(json!({"id": "e632b7095b0bf32c260fa4c539e9fd7b852d0de454e9be26f24d0d6f91d069d3", "event_type": "resolve"})))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("pagertree://{INTEGRATION_ID}?action=map")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Success))).await.unwrap());
  }

  #[test]
  fn test_actions() {
    // The default creates a new alert with a new ID every time
    let obj = pagertree(&format!("pagertree://{INTEGRATION_ID}"));
    let first = obj.payload(&ctx("Disk full", "db01", NotifyType::Success));
    assert_eq!(first["event_type"], "create");
    assert_ne!(obj.payload(&ctx("Disk full", "db01", NotifyType::Info))["id"], first["id"]);
    // No title uses the asset description
    assert_eq!(obj.payload(&ctx("", "db01", NotifyType::Info))["title"], "Apprise Notifications");

    // Map creates on failure and resolves on success
    let obj = pagertree(&format!("pagertree://{INTEGRATION_ID}?action=map"));
    for (notify_type, event) in
      [(NotifyType::Info, "create"), (NotifyType::Warning, "create"), (NotifyType::Failure, "create"), (NotifyType::Success, "resolve")]
    {
      assert_eq!(obj.payload(&ctx("Disk full", "db01", notify_type))["event_type"], event);
    }

    // The same title gives the same ID, so the success resolves the alert
    // the failure created
    let created = obj.payload(&ctx("Disk full", "at 95%", NotifyType::Failure));
    assert_eq!(created["title"], "Disk full");
    let resolved = obj.payload(&ctx("Disk full", "normal", NotifyType::Success));
    assert_eq!(resolved["id"], created["id"]);
    assert!(resolved.get("title").is_none());

    // Without a title, the first line of the message is used
    assert_eq!(obj.payload(&ctx("", "Disk full\nat 95%", NotifyType::Info))["id"], created["id"]);
    assert_ne!(obj.payload(&ctx("", "one", NotifyType::Info))["id"], obj.payload(&ctx("", "two", NotifyType::Info))["id"]);

    // A resolve without its own ID uses the title as well
    let obj = pagertree(&format!("pagertree://{INTEGRATION_ID}?action=resolve"));
    assert_eq!(obj.payload(&ctx("Disk full", "normal", NotifyType::Info))["id"], created["id"]);

    // ?dedup= sets the ID, the same as ?tid=
    let obj = pagertree(&format!("pagertree://{INTEGRATION_ID}?action=map&dedup=disk-full"));
    assert_eq!(obj.payload(&ctx("", "db01", NotifyType::Failure))["id"], "disk-full");

    // Payload extras override what we built
    let obj = pagertree(&format!("pagertree://{INTEGRATION_ID}?action=acknowledge&:event_type=resolve"));
    assert_eq!(obj.payload(&ctx("T", "B", NotifyType::Info))["event_type"], "resolve");
  }

  #[tokio::test]
  async fn test_retry_keeps_id() {
    // The retry reuses the ID so PagerTree sees one alert
    let server = MockServer::start().await;
    let route = format!("/integration/{INTEGRATION_ID}");
    Mock::given(method("POST")).and(path(route.clone())).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path(route.clone())).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("pagertree://{INTEGRATION_ID}?retry=1&wait=0")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "Server down", NotifyType::Info))).await.is_success());
    let requests = server.received_requests().await.unwrap();
    let ids: Vec<Value> = requests.iter().filter(|r| r.url.path() == route).map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["id"].clone()).collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path(format!("/integration/{INTEGRATION_ID}")))
        .respond_with(ResponseTemplate::new(status))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url(&format!("pagertree://{INTEGRATION_ID}")).unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b", NotifyType::Info))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }

  #[test]
  fn test_service_details() {
    let details = PagerTree::static_details();
    assert_eq!(details.service_name, "PagerTree");
    assert_eq!(details.protocols, vec!["pagertree"]);
  }
}
