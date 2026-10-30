use crate::asset::NotifyImageSize;
use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::qsd_bool;
use crate::utils::qsd::validate_token;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `PAGERDUTY_SEVERITIES`, in the order a partial `?severity=` is
/// matched against.
const SEVERITIES: &[&str] = &["info", "warning", "critical", "error"];

/// Upstream `PAGERDUTY_ACTIONS`.
const ACTIONS: &[&str] = &["trigger", "map", "resolve"];

/// The Events API v2 endpoint of each region.
const REGIONS: &[(&str, &str)] = &[("us", "https://events.pagerduty.com"), ("eu", "https://events.eu.pagerduty.com")];

/// Upstream `PAGERDUTY_SEVERITY_MAP`.
fn severity_for(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    NotifyType::Info | NotifyType::Success => "info",
    NotifyType::Warning => "warning",
    NotifyType::Failure => "critical",
  }
}

pub struct PagerDuty {
  apikey: String,
  integration_key: String,
  source: String,
  component: String,
  group: Option<String>,
  class_id: Option<String>,
  click: Option<String>,
  details: Vec<(String, String)>,
  region: &'static str,
  severity: Option<&'static str>,
  include_image: bool,
  action: &'static str,
  dedup: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl PagerDuty {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // pagerduty://{integrationkey}@{apikey}/{source}/{component}
    let apikey = arg(url, "apikey").or_else(|| url.host.clone()).and_then(|k| validate_token(&k));
    let Some(apikey) = apikey else {
      tracing::warn!("An invalid Pager Duty API Key was specified.");
      return None;
    };
    let integration_key = arg(url, "integrationkey").or_else(|| url.user.clone()).and_then(|k| validate_token(&k));
    let Some(integration_key) = integration_key else {
      tracing::warn!("An invalid Pager Duty Routing Key was specified.");
      return None;
    };

    // The source and component come from the path when not given as arguments
    let mut path = url.path_parts.iter();
    let source = arg(url, "source").or_else(|| path.next().cloned());
    let component = arg(url, "component").or_else(|| path.next().cloned());
    let source = match source.filter(|s| !s.is_empty()) {
      Some(s) => match validate_token(&s) {
        Some(s) => s,
        None => {
          tracing::warn!("An invalid Pager Duty Notification Source ({}) was specified.", s);
          return None;
        }
      },
      None => "Apprise".to_string(),
    };
    let component = match component.filter(|c| !c.is_empty()) {
      Some(c) => match validate_token(&c) {
        Some(c) => c,
        None => {
          tracing::warn!("An invalid Pager Duty Notification Component ({}) was specified.", c);
          return None;
        }
      },
      None => "Notification".to_string(),
    };

    let region = match arg(url, "region") {
      Some(r) => match REGIONS.iter().find(|(name, _)| *name == r.to_lowercase()) {
        Some((name, _)) => *name,
        None => {
          tracing::warn!("The PagerDuty region specified ({}) is invalid.", r);
          return None;
        }
      },
      None => "us",
    };

    // Upstream compares the value as given, so a partial name must be
    // lower case
    let severity = match arg(url, "severity") {
      Some(s) => match incident::match_prefix(SEVERITIES, &s) {
        Some(s) => Some(s),
        None => {
          tracing::warn!("The PagerDuty severity specified ({}) is invalid.", s);
          return None;
        }
      },
      None => None,
    };

    let action = match arg(url, "action") {
      Some(a) => match incident::match_choice(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The PagerDuty action specified ({}) is invalid.", a);
          return None;
        }
      },
      None => "trigger",
    };

    Some(Self {
      apikey,
      integration_key,
      source,
      component,
      group: arg(url, "group"),
      class_id: arg(url, "class"),
      click: arg(url, "click"),
      details: incident::unquoted_map(&url.qsd_add),
      region,
      severity,
      include_image: qsd_bool(url, "image", true),
      action,
      // A fixed key lets several notifications update the same incident
      dedup: arg(url, "dedup").map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pager Duty",
      service_url: Some("https://pagerduty.com/"),
      setup_url: Some("https://appriseit.com/services/pagerduty/"),
      protocols: vec!["pagerduty"],
      description: "Raise, update and resolve PagerDuty incidents through the Events API v2.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // Work out whether we raise or resolve the incident
    let resolve = self.action == "resolve" || (self.action == "map" && ctx.notify_type == NotifyType::Success);

    // Our own key always wins. Otherwise map and resolve need a key that
    // stays the same between notifications, so a later success can find
    // the incident a failure raised. The title is merged into the first
    // line of the body, so that line stands in for it.
    let dedup = self.dedup.clone().or_else(|| (self.action != "trigger").then(|| incident::sha256_hex(incident::first_line(&ctx.body))));

    if resolve {
      // A resolve only names the incident, as in PagerDuty's own example
      return json!({
        "routing_key": self.integration_key,
        "event_action": "resolve",
        "dedup_key": dedup,
      });
    }

    let mut payload = json!({
      "routing_key": self.integration_key,
      "payload": {
        "summary": ctx.body,
        "severity": self.severity.unwrap_or_else(|| severity_for(&ctx.notify_type)),
        "source": self.source,
        "component": self.component,
      },
      "client": ctx.asset.app_id,
      "event_action": "trigger",
    });
    if let Some(dedup) = dedup {
      payload["dedup_key"] = json!(dedup);
    }
    if let Some(group) = &self.group {
      payload["payload"]["group"] = json!(group);
    }
    if let Some(class_id) = &self.class_id {
      payload["payload"]["class"] = json!(class_id);
    }
    if let Some(click) = &self.click {
      payload["links"] = json!([{ "href": click }]);
    }
    if self.include_image
      && let Some(image) = ctx.image_url(NotifyImageSize::Xy128)
    {
      payload["images"] = json!([{ "src": image, "alt": ctx.notify_type.as_str() }]);
    }
    if !self.details.is_empty() {
      payload["payload"]["custom_details"] = incident::json_object(&self.details);
    }
    payload
  }
}

#[async_trait]
impl Notify for PagerDuty {
  fn schemas(&self) -> &[&str] {
    &["pagerduty"]
  }
  fn service_name(&self) -> &str {
    "Pager Duty"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The body is the event summary, which PagerDuty limits to 1024 characters
  fn body_maxlen(&self) -> usize {
    1024
  }
  // PagerDuty has no title; it is merged into the summary
  fn title_maxlen(&self) -> usize {
    0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["pagerduty".into(), self.integration_key.clone(), self.apikey.clone(), self.source.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = REGIONS.iter().find(|(name, _)| *name == self.region).map(|(_, base)| *base).unwrap_or(REGIONS[0].1);
    let client = build_client(self.verify_certificate)?;
    let request = client
      .post(format!("{}/v2/enqueue", service_base(base)))
      .header("User-Agent", APP_ID)
      .header("Authorization", format!("Token token={}", self.apikey))
      .json(&self.payload(ctx));
    incident::exchange(request, "Pager Duty notification", &[200, 201, 202], &[]).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn pagerduty(url: &str) -> PagerDuty {
    PagerDuty::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "pagerduty://myroutekey@myapikey",
      "pagerduty://myroutekey@myapikey?image=no",
      "pagerduty://myroutekey@myapikey?region=eu",
      "pagerduty://myroutekey@myapikey?region=EU",
      "pagerduty://myroutekey@myapikey?severity=critical",
      "pagerduty://myroutekey@myapikey?severity=err",
      "pagerduty://myroutekey@myapikey?+key=value&+key2=value2",
      "pagerduty://myroutekey@myapikey/mysource/mycomponent",
      "pagerduty://routekey@apikey/ms/mc?group=mygroup&class=myclass",
      "pagerduty://?integrationkey=r&apikey=a&source=s&component=c&group=g&class=c&image=no&click=http://localhost",
      "pagerduty://routekey@apikey/?action=map&dedup=disk-full",
      "pagerduty://routekey@apikey/?action=resolve",
      "pagerduty://somerkey@someapikey/bizarre/code",
    ];
    for url in urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "pagerduty://",
      "pagerduty://%20@%20/",
      "pagerduty://%20/",
      "pagerduty://%20@abcd/",
      "pagerduty://myroutekey@myapikey/%20",
      "pagerduty://myroutekey@myapikey/mysource/%20",
      "pagerduty://myroutekey@myapikey?region=invalid",
      "pagerduty://myroutekey@myapikey?severity=invalid",
      // Upstream matches the severity as given
      "pagerduty://myroutekey@myapikey?severity=CRIT",
      "pagerduty://routekey@apikey/?action=invalid",
    ];
    for url in urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // The user is the routing (integration) key and the host the API key
    let obj = pagerduty("pagerduty://myroutekey@myapikey");
    assert_eq!(obj.integration_key, "myroutekey");
    assert_eq!(obj.apikey, "myapikey");
    assert_eq!(obj.region, "us");
    assert_eq!(obj.source, "Apprise");
    assert_eq!(obj.component, "Notification");
    assert_eq!(obj.action, "trigger");
    assert!(obj.include_image);
    assert!(obj.dedup.is_none() && obj.severity.is_none());

    let obj = pagerduty("pagerduty://?integrationkey=r&apikey=a&source=s&component=c&group=g&class=cl&image=no&click=http://localhost");
    assert_eq!((obj.integration_key.as_str(), obj.apikey.as_str()), ("r", "a"));
    assert_eq!((obj.source.as_str(), obj.component.as_str()), ("s", "c"));
    assert_eq!(obj.group.as_deref(), Some("g"));
    assert_eq!(obj.class_id.as_deref(), Some("cl"));
    assert_eq!(obj.click.as_deref(), Some("http://localhost"));
    assert!(!obj.include_image);

    let obj = pagerduty("pagerduty://myroutekey@myapikey/mysource/mycomponent?severity=err&region=EU&action=RES&dedup=%20disk-full%20");
    assert_eq!((obj.source.as_str(), obj.component.as_str()), ("mysource", "mycomponent"));
    assert_eq!(obj.severity, Some("error"));
    assert_eq!(obj.region, "eu");
    assert_eq!(obj.action, "resolve");
    assert_eq!(obj.dedup.as_deref(), Some("disk-full"));
    assert_eq!(pagerduty("pagerduty://r@a?action=m").action, "map");
  }

  #[test]
  fn test_service_details() {
    let details = PagerDuty::static_details();
    assert_eq!(details.service_name, "Pager Duty");
    assert_eq!(details.protocols, vec!["pagerduty"]);
    let svc = from_url("pagerduty://myroutekey@myapikey").unwrap();
    assert_eq!(svc.body_maxlen(), 1024);
    assert_eq!(svc.title_maxlen(), 0);
  }

  #[tokio::test]
  async fn test_send_trigger() {
    // Matches pycapture.py for this URL with --type failure
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/enqueue"))
      .and(header("authorization", "Token token=apikey"))
      .and(header("content-type", "application/json"))
      .and(body_json(json!({
        "routing_key": "routingkey",
        "payload": {
          "summary": "T\r\nB",
          "severity": "critical",
          "source": "source",
          "component": "component",
          "custom_details": {"trace": "abc"},
        },
        "client": "Apprise",
        "event_action": "trigger",
        "images": [{
          "src": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-failure-128x128.png",
          "alt": "failure",
        }],
      })))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("pagerduty://routingkey@apikey/source/component?+trace=abc").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "T\r\nB", NotifyType::Failure))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_golden_default() {
    // apprise-go golden "default"
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/enqueue"))
      .and(body_json(json!({
        "routing_key": "routingkey",
        "payload": {
          "summary": "apprise parity\r\nhello from python",
          "severity": "error",
          "source": "source",
          "component": "component",
          "group": "ops",
          "class": "service",
          "custom_details": {"trace": "abc"},
        },
        "client": "Apprise",
        "event_action": "trigger",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let mut a = crate::Apprise::new();
    assert!(a.add("pagerduty://routingkey@apikey/source/component?region=eu&severity=error&image=no&group=ops&class=service&+trace=abc"));
    let ctx = ctx("apprise parity", "hello from python", NotifyType::Warning);
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
  }

  #[test]
  fn test_actions() {
    let sent = |url: &str, title: &str, body: &str, notify_type: NotifyType| {
      let svc = pagerduty(url);
      let ctx = ctx("", &format!("{}\r\n{}", title, body), notify_type);
      svc.payload(&ctx)
    };

    // The default always raises an incident and sends no key
    let p = sent("pagerduty://routekey@apikey", "Disk full", "db01", NotifyType::Success);
    assert_eq!(p["event_action"], "trigger");
    assert!(p.get("dedup_key").is_none());

    // A fixed key is sent with every event
    let p = sent("pagerduty://routekey@apikey?dedup=disk-full", "", "db01", NotifyType::Failure);
    assert_eq!(p["event_action"], "trigger");
    assert_eq!(p["dedup_key"], "disk-full");

    // Map raises on failure and resolves on success
    for (notify_type, event) in
      [(NotifyType::Info, "trigger"), (NotifyType::Warning, "trigger"), (NotifyType::Failure, "trigger"), (NotifyType::Success, "resolve")]
    {
      assert_eq!(sent("pagerduty://routekey@apikey?action=map", "Disk full", "db01", notify_type)["event_action"], event);
    }

    // The same title gives the same key, so the success resolves the
    // incident the failure raised
    let raised = sent("pagerduty://routekey@apikey?action=map", "Disk full", "at 95%", NotifyType::Failure);
    assert!(raised["payload"]["summary"].as_str().unwrap().starts_with("Disk full"));
    let resolved = sent("pagerduty://routekey@apikey?action=map", "Disk full", "back to normal", NotifyType::Success);
    assert_eq!(resolved["dedup_key"], raised["dedup_key"]);
    // A resolve only names the incident
    assert_eq!(resolved, json!({"routing_key": "routekey", "event_action": "resolve", "dedup_key": raised["dedup_key"]}));

    // A different title is a different incident
    let other = sent("pagerduty://routekey@apikey?action=map", "CPU hot", "db01", NotifyType::Info);
    assert_ne!(other["dedup_key"], raised["dedup_key"]);

    // Resolve ignores the notification type
    assert_eq!(sent("pagerduty://routekey@apikey?action=RES", "", "db01", NotifyType::Failure)["event_action"], "resolve");
  }

  /// apprise-go goldens `upstream210-options-*`: title `Alert`, body
  /// `body\nsecond line`, through the whole pipeline.
  #[tokio::test]
  async fn test_send_golden_options() {
    let cases = [
      (
        "pagerduty://routing@key?action=map",
        NotifyType::Success,
        json!({
          "routing_key": "routing",
          "event_action": "resolve",
          "dedup_key": "44a57b22e03d9a661c22ffc5ffea51b3e1e33de24a53609e70f690dea8f70872",
        }),
      ),
      (
        "pagerduty://routing@key?action=resolve",
        NotifyType::Failure,
        json!({
          "routing_key": "routing",
          "event_action": "resolve",
          "dedup_key": "44a57b22e03d9a661c22ffc5ffea51b3e1e33de24a53609e70f690dea8f70872",
        }),
      ),
      (
        "pagerduty://routing@key?dedup=explicit",
        NotifyType::Success,
        json!({
          "routing_key": "routing",
          "payload": {
            "summary": "Alert\r\nbody\nsecond line",
            "severity": "info",
            "source": "Apprise",
            "component": "Notification",
          },
          "client": "Apprise",
          "event_action": "trigger",
          "dedup_key": "explicit",
          "images": [{
            "src": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-success-128x128.png",
            "alt": "success",
          }],
        }),
      ),
    ];
    for (url, notify_type, expected) in cases {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v2/enqueue")).and(body_json(expected)).respond_with(ResponseTemplate::new(202)).expect(1).mount(&server).await;
      let mut a = crate::Apprise::new();
      assert!(a.add(url));
      let ctx = ctx("Alert", "body\nsecond line", notify_type);
      assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success(), "{}", url);
    }
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v2/enqueue")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url("pagerduty://somerkey@someapikey/bizarre/code").unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "body", NotifyType::Info))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }
}
