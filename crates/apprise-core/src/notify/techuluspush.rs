use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::is_uuid4;
use async_trait::async_trait;
use serde_json::json;

pub struct TechulusPush {
  apikey: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl TechulusPush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // API keys are UUID4 strings
    // A YAML `apikey:` replaces the URL's (upstream passes it to the
    // constructor)
    let apikey = url.kwarg("apikey").or(url.host.as_deref()).unwrap_or_default().trim().to_string();
    if !is_uuid4(&apikey) {
      tracing::warn!("An invalid Techulus Push API key was specified.");
      return None;
    }
    Some(Self { apikey, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Techulus Push",
      service_url: Some("https://push.techulus.com"),
      setup_url: Some("https://appriseit.com/services/techulus/"),
      protocols: vec!["push"],
      description: "Send push via Techulus.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for TechulusPush {
  fn schemas(&self) -> &[&str] {
    &["push"]
  }
  fn service_name(&self) -> &str {
    "Techulus Push"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    1000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = json!({ "title": ctx.title, "body": ctx.body });
    let url = format!("{}/api/v1/notify", service_base("https://push.techulus.com"));
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("x-api-key", &self.apikey)
      .body(serde_json::to_vec(&payload)?)
      .send()
      .await?;
    let status = resp.status();
    if matches!(status.as_u16(), 200 | 204) {
      tracing::info!("Sent Techulus Push notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send Techulus Push notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const UUID4: &str = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";

  #[test]
  fn test_urls() {
    assert!(from_url(&format!("push://{}", UUID4)).is_some());
    // Upstream has no techulus:// schema
    assert!(from_url(&format!("techulus://{}", UUID4)).is_none());
    for url in ["push://", "push://:@/", "push://abcd", "push://8b799edf-6f98-3d3a-9be7-2862fb4e5752"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/notify"))
      .and(header("x-api-key", UUID4))
      .and(header("content-type", "application/json"))
      .and(body_json(json!({"title": "T", "body": "B"})))
      .respond_with(ResponseTemplate::new(204))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("push://{}", UUID4)).unwrap();
    assert_eq!(svc.body_maxlen(), 1000);
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/api/v1/notify")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), from_url(&format!("push://{}", UUID4)).unwrap().send(&ctx)).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = TechulusPush::static_details();
    assert_eq!(details.service_name, "Techulus Push");
    assert!(details.protocols.contains(&"push"));
  }

  /// A YAML `apikey:` replaces the URL's, as upstream's constructor
  /// argument does (upstream: x-api-key 11111111-2222-4333-8444-...).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_apikey_replaces_the_urls() {
    let other = "11111111-2222-4333-8444-555555555555";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1/notify"))
      .and(header("x-api-key", other))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let doc = format!("urls:\n  - push://{UUID4}:\n      apikey: {other}\n");
    let (a, _) = crate::config::yaml::load_for_test(&doc).await;
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
  }
}
