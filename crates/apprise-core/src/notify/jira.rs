//! Jira Service Management, through the alert API it took over from
//! Opsgenie:
//!
//!   jira://[{user}@]{apikey}/[@{user}][/#{team}][/*{schedule}][/^{escalation}]
//!
//! Everything but the endpoint is shared with [`super::opsgenie`]. See
//! <https://developer.atlassian.com/cloud/jira/service-desk-ops/rest/v1/api-group-integration-events/>.

use crate::error::NotifyError;
use crate::notify::opsgenie::{Flavor, Opsgenie};
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

pub struct Jira(Opsgenie);

impl Jira {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    Opsgenie::parse(url, Flavor::Jira).map(Self)
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Jira",
      service_url: Some("https://atlassian.com/"),
      setup_url: Some("https://github.com/caronc/apprise/wiki/Notify_jira"),
      protocols: vec!["jira"],
      description: "Create, note, acknowledge, close and delete Jira Service Management alerts.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Jira {
  fn schemas(&self) -> &[&str] {
    &["jira"]
  }
  fn service_name(&self) -> &str {
    "Jira"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.0.tags()
  }
  fn body_maxlen(&self) -> usize {
    self.0.body_maxlen()
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(self.0.identifier())
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    self.0.send_alerts(ctx).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::opsgenie::tests::{GOOD_RESPONSE, ctx, invalid_urls, store_round_trip, valid_urls};
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use serde_json::json;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const ALERTS: &str = "/jsm/ops/integration/v2/alerts";

  #[test]
  fn test_valid_urls() {
    for url in valid_urls("jira") {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in invalid_urls("jira") {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_send_new() {
    // Matches pycapture.py, including upstream's "Accepts" header; both
    // regions use the one Atlassian endpoint
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(ALERTS))
      .and(header("authorization", "GenieKey apikey"))
      .and(header("accepts", "application/json"))
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
      .expect(2)
      .mount(&server)
      .await;
    for region in ["us", "eu"] {
      let svc =
        from_url(&format!("jira://joe@apikey/@user/%23team/*sched/^esc?priority=high&tags=b,a&entity=E&alias=A&+key=value&batch=yes&region={region}")).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Warning))).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_store_round_trip() {
    store_round_trip("jira", ALERTS).await;
  }

  #[test]
  fn test_service_details() {
    let svc = from_url("jira://apikey").unwrap();
    assert_eq!(svc.service_name(), "Jira");
    assert_eq!(svc.body_maxlen(), 15000);
    assert_eq!(Jira::static_details().protocols, vec!["jira"]);
  }
}
