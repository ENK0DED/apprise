//! Spike.sh, through an alert integration:
//!
//!   spike://{integration_key}
//!
//! The integration's own URL (`https://api.spike.sh/v1/alerts/{key}`) is
//! accepted too. See <https://www.spike.sh/docs/alerts/send-alerts-to-spike/>.

use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct Spike {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `^[a-z0-9]{32}$` (case-insensitive).
fn is_token(token: &str) -> bool {
  token.len() == 32 && token.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Spike {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = arg(url, "token").or_else(|| url.host.clone()).unwrap_or_default();
    if !is_token(&token) {
      tracing::warn!("The Spike.sh integration key specified is invalid.");
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Spike.sh",
      service_url: Some("https://www.spike.sh/"),
      setup_url: Some("https://appriseit.com/services/spike/"),
      protocols: vec!["spike"],
      description: "Send alerts to Spike.sh.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Spike {
  fn schemas(&self) -> &[&str] {
    &["spike"]
  }
  fn service_name(&self) -> &str {
    "Spike.sh"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["spike".into(), self.token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = json!({
      "message": if ctx.title.is_empty() { &ctx.body } else { &ctx.title },
      "description": ctx.body,
    });
    let client = build_client(self.verify_certificate)?;
    let request = client.post(format!("{}/v1/alerts/{}", service_base("https://api.spike.sh"), self.token)).header("User-Agent", APP_ID).json(&payload);
    incident::exchange(request, "Spike.sh notification", &[200], &[]).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "1234567890abcdef1234567890abcdef";

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type: NotifyType::Info, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls =
      [format!("spike://{TOKEN}"), format!("spike://?token={TOKEN}"), format!("https://api.spike.sh/v1/alerts/{TOKEN}"), format!("spike://{}", "F".repeat(32))];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = ["spike://".to_string(), "spike://invalid-key".to_string(), format!("spike://{}", "a".repeat(31)), format!("spike://{}-", "a".repeat(31))];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_send() {
    // Matches pycapture.py: the body stands in for a missing title
    for (title, message) in [("", "B"), ("T", "T")] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path(format!("/v1/alerts/{TOKEN}")))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({"message": message, "description": "B"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url(&format!("https://api.spike.sh/v1/alerts/{TOKEN}")).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx(title, "B"))).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [202, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(format!("/v1/alerts/{TOKEN}"))).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("spike://{TOKEN}")).unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await;
      assert!(matches!(result, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
  }

  #[test]
  fn test_service_details() {
    let details = Spike::static_details();
    assert_eq!(details.service_name, "Spike.sh");
    assert_eq!(details.protocols, vec!["spike"]);
  }
}
