use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::parse_bool_or;
use crate::utils::qsd::validate_token;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `SIGNL4_ACTIONS`, in the order a partial `?action=` is matched.
const ACTIONS: &[&str] = &["trigger", "map", "acknowledge", "resolve"];

pub struct Signl4 {
  secret: String,
  service: Option<String>,
  location: Option<String>,
  alerting_scenario: Option<String>,
  filtering: bool,
  external_id: Option<String>,
  status: Option<String>,
  action: &'static str,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Signl4 {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let Some(secret) = arg(url, "secret").or_else(|| url.host.clone()).and_then(|s| validate_token(&s)) else {
      tracing::warn!("An invalid SIGNL4 team or integration secret was specified.");
      return None;
    };

    let action = match arg(url, "action") {
      Some(a) => match incident::match_choice(ACTIONS, &a) {
        Some(a) => a,
        None => {
          tracing::warn!("The SIGNL4 action specified ({}) is invalid.", a);
          return None;
        }
      },
      None => "trigger",
    };

    Some(Self {
      secret,
      service: arg(url, "service"),
      location: arg(url, "location"),
      alerting_scenario: arg(url, "alerting_scenario"),
      filtering: arg(url, "filtering").is_some_and(|f| parse_bool_or(Some(&f), false)),
      // ?dedup= is the name the other on-call plugins use for this ID
      external_id: arg(url, "external_id").or_else(|| arg(url, "dedup")),
      status: arg(url, "status"),
      action,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SIGNL4",
      service_url: Some("https://signl4.com/"),
      setup_url: Some("https://appriseit.com/services/signl4/"),
      protocols: vec!["signl4"],
      description: "Raise, acknowledge and resolve SIGNL4 mobile alerts.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    // A status given directly is passed along as-is. Otherwise our action
    // decides it, and a new alert needs no status at all.
    let status = self.status.clone().or_else(|| match self.action {
      "resolve" => Some("resolved".into()),
      "map" if ctx.notify_type == NotifyType::Success => Some("resolved".into()),
      "acknowledge" => Some("acknowledged".into()),
      _ => None,
    });

    // Our own ID always wins. Every action but trigger needs one that stays
    // the same between notifications; the title (or the first line of the
    // body when there is none) stands in for it.
    let external_id = self.external_id.clone().or_else(|| (self.action != "trigger").then(|| incident::title_key(&ctx.title, &ctx.body)));

    let mut payload = json!({
      "title": if ctx.title.is_empty() { ctx.asset.app_desc.as_str() } else { ctx.title.as_str() },
      "body": ctx.body,
      "X-S4-SourceSystem": ctx.asset.app_id,
    });
    if let Some(service) = &self.service {
      payload["X-S4-Service"] = json!(service);
    }
    if let Some(scenario) = &self.alerting_scenario {
      payload["X-S4-AlertingScenario"] = json!(scenario);
    }
    if let Some(location) = &self.location {
      payload["X-S4-Location"] = json!(location);
    }
    if self.filtering {
      payload["X-S4-Filtering"] = json!(true);
    }
    if let Some(id) = external_id {
      payload["X-S4-ExternalID"] = json!(id);
    }
    if let Some(status) = status {
      payload["X-S4-Status"] = json!(status);
    }
    payload
  }
}

#[async_trait]
impl Notify for Signl4 {
  fn schemas(&self) -> &[&str] {
    &["signl4"]
  }
  fn service_name(&self) -> &str {
    "SIGNL4"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["signl4".into(), self.secret.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/webhook/{}/", service_base("https://connect.signl4.com"), self.secret);
    let request = client.post(url).header("User-Agent", APP_ID).json(&self.payload(ctx));
    incident::exchange(request, "SIGNL4 notification", &[200, 201, 202], &[]).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn signl4(url: &str) -> Signl4 {
    Signl4::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "signl4://secret/",
      "signl4://?secret=secret",
      "signl4://secret/?service=IoT",
      "signl4://secret/?filtering=yes",
      "signl4://secret/?location=40.6413111,-73.7781391",
      "signl4://secret/?alerting_scenario=singl4_ack",
      "signl4://secret/?filtering=False",
      "signl4://secret/?external_id=ar1234&status=new",
      "signl4://secret/?service=IoT&location=40.6413111,-73.7781391&alerting_scenario=multi_ack&filtering=yes&external_id=ar1234&status=new",
      "signl4://secret?action=map",
      "signl4://secret?action=ack",
    ];
    for url in urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["signl4://", "signl4://:@/", "signl4://%20%20/", "signl4://secret?action=invalid"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = signl4("signl4://secret/?service=IoT&location=40.6413111,-73.7781391&alerting_scenario=multi_ack&filtering=yes&external_id=ar1234&status=new");
    assert_eq!(obj.secret, "secret");
    assert_eq!(obj.service.as_deref(), Some("IoT"));
    assert_eq!(obj.location.as_deref(), Some("40.6413111,-73.7781391"));
    assert_eq!(obj.alerting_scenario.as_deref(), Some("multi_ack"));
    assert!(obj.filtering);
    assert_eq!(obj.external_id.as_deref(), Some("ar1234"));
    assert_eq!(obj.status.as_deref(), Some("new"));
    assert_eq!(obj.action, "trigger");
    assert_eq!(signl4("signl4://?secret=other").secret, "other");
    assert_eq!(signl4("signl4://secret?action=ack").action, "acknowledge");
    assert_eq!(signl4("signl4://secret?action=R").action, "resolve");
    // ?dedup= sets the ID, the same as ?external_id=
    assert_eq!(signl4("signl4://secret?action=map&dedup=disk-full").external_id.as_deref(), Some("disk-full"));
    assert_eq!(signl4("signl4://secret?dedup=b&external_id=a").external_id.as_deref(), Some("a"));
  }

  #[test]
  fn test_filtering_parsing() {
    // An escaped percent sign reaches the plugin still encoded and is
    // decoded once more, as upstream does
    assert!(signl4("signl4://secret/?filtering=%2579es").filtering);
    // An unparseable value falls back on the default
    assert!(!signl4("signl4://secret/?filtering=%25zz").filtering);
    assert!(!signl4("signl4://secret/?filtering=False").filtering);
  }

  #[test]
  fn test_actions() {
    // The default raises a new alert with no status or ID
    let obj = signl4("signl4://secret");
    let p = obj.payload(&ctx("Disk full", "db01", NotifyType::Success));
    assert!(p.get("X-S4-Status").is_none() && p.get("X-S4-ExternalID").is_none());

    // Map raises on failure and resolves on success
    let obj = signl4("signl4://secret?action=map");
    for (notify_type, status) in [(NotifyType::Info, None), (NotifyType::Warning, None), (NotifyType::Failure, None), (NotifyType::Success, Some("resolved"))] {
      assert_eq!(obj.payload(&ctx("Disk full", "db01", notify_type)).get("X-S4-Status").and_then(|s| s.as_str()), status);
    }

    // The same title gives the same ID, so the success resolves the alert
    // the failure raised
    let raised = obj.payload(&ctx("Disk full", "at 95%", NotifyType::Failure))["X-S4-ExternalID"].clone();
    assert_eq!(obj.payload(&ctx("Disk full", "normal", NotifyType::Success))["X-S4-ExternalID"], raised);
    // Without a title, the first line of the message is used
    assert_eq!(obj.payload(&ctx("", "Disk full\nat 95%", NotifyType::Info))["X-S4-ExternalID"], raised);
    assert_ne!(obj.payload(&ctx("", "one", NotifyType::Info))["X-S4-ExternalID"], obj.payload(&ctx("", "two", NotifyType::Info))["X-S4-ExternalID"]);

    // Acknowledge and resolve ignore the notification type
    let p = signl4("signl4://secret?action=ack").payload(&ctx("Disk full", "db01", NotifyType::Info));
    assert_eq!(p["X-S4-Status"], "acknowledged");
    assert_eq!(p["X-S4-ExternalID"], raised);
    assert_eq!(signl4("signl4://secret?action=resolve").payload(&ctx("Disk full", "db01", NotifyType::Failure))["X-S4-Status"], "resolved");

    // A status given directly always wins over the action
    let p = signl4("signl4://secret?action=map&status=new&external_id=a1").payload(&ctx("", "db01", NotifyType::Success));
    assert_eq!(p["X-S4-Status"], "new");
    assert_eq!(p["X-S4-ExternalID"], "a1");
  }

  /// apprise-go goldens, captured from upstream 2.1.0.
  #[tokio::test]
  async fn test_send_goldens() {
    let hash = "44a57b22e03d9a661c22ffc5ffea51b3e1e33de24a53609e70f690dea8f70872";
    let cases = [
      ("signl4://secret123", "title", "body", NotifyType::Info, json!({"title": "title", "body": "body", "X-S4-SourceSystem": "Apprise"})),
      (
        "signl4://secret123?service=svc&location=loc&alerting_scenario=alert&filtering=yes&external_id=ext&status=ok",
        "",
        "hello",
        NotifyType::Warning,
        json!({
          "title": "Apprise Notifications",
          "body": "hello",
          "X-S4-SourceSystem": "Apprise",
          "X-S4-Service": "svc",
          "X-S4-AlertingScenario": "alert",
          "X-S4-Location": "loc",
          "X-S4-Filtering": true,
          "X-S4-ExternalID": "ext",
          "X-S4-Status": "ok",
        }),
      ),
      (
        "signl4://secret123?action=map",
        "Alert",
        "body\nsecond line",
        NotifyType::Success,
        json!({"title": "Alert", "body": "body\nsecond line", "X-S4-SourceSystem": "Apprise", "X-S4-ExternalID": hash, "X-S4-Status": "resolved"}),
      ),
      (
        "signl4://secret123?action=resolve",
        "Alert",
        "body\nsecond line",
        NotifyType::Failure,
        json!({"title": "Alert", "body": "body\nsecond line", "X-S4-SourceSystem": "Apprise", "X-S4-ExternalID": hash, "X-S4-Status": "resolved"}),
      ),
      (
        "signl4://secret123?action=acknowledge",
        "Alert",
        "body\nsecond line",
        NotifyType::Success,
        json!({"title": "Alert", "body": "body\nsecond line", "X-S4-SourceSystem": "Apprise", "X-S4-ExternalID": hash, "X-S4-Status": "acknowledged"}),
      ),
      (
        "signl4://secret123?action=map&dedup=explicit",
        "Alert",
        "body\nsecond line",
        NotifyType::Failure,
        json!({"title": "Alert", "body": "body\nsecond line", "X-S4-SourceSystem": "Apprise", "X-S4-ExternalID": "explicit"}),
      ),
    ];
    for (url, title, body, notify_type, expected) in cases {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/webhook/secret123/"))
        .and(header("content-type", "application/json"))
        .and(body_json(expected))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url(url).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx(title, body, notify_type))).await.unwrap(), "{}", url);
    }
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/webhook/secret/")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url("signl4://secret/").unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b", NotifyType::Failure))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }

  #[test]
  fn test_service_details() {
    let details = Signl4::static_details();
    assert_eq!(details.service_name, "SIGNL4");
    assert_eq!(details.protocols, vec!["signl4"]);
  }
}
