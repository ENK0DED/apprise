use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::is_uuid4;
use async_trait::async_trait;
use serde_json::json;

pub struct Kumulos {
  apikey: String,
  serverkey: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Kumulos {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // kumulos://{apikey}/{serverkey}: a UUID4 API key and a 36 character
    // server key
    // A YAML `apikey:` replaces the URL's (upstream passes it to the
    // constructor)
    let apikey = url.kwarg("apikey").or(url.host.as_deref()).unwrap_or_default().trim().to_string();
    if !is_uuid4(&apikey) {
      tracing::warn!("An invalid Kumulos API Key was specified.");
      return None;
    }
    let serverkey = url.path_parts.first().map(|k| k.trim().to_string()).unwrap_or_default();
    if serverkey.len() != 36 || !serverkey.chars().all(|c| c.is_ascii_alphanumeric() || c == '+') {
      tracing::warn!("An invalid Kumulos Server Key was specified.");
      return None;
    }
    Some(Self { apikey, serverkey, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Kumulos",
      service_url: Some("https://kumulos.com/"),
      setup_url: Some("https://appriseit.com/services/kumulos/"),
      protocols: vec!["kumulos"],
      description: "Send push notifications via Kumulos.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Kumulos {
  fn schemas(&self) -> &[&str] {
    &["kumulos"]
  }
  fn service_name(&self) -> &str {
    "Kumulos"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    64
  }
  fn body_maxlen(&self) -> usize {
    240
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = json!({
      "target": { "broadcast": true },
      "content": { "title": ctx.title, "message": ctx.body },
    });
    let url = format!("{}/v2/notifications", service_base("https://messages.kumulos.com"));
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("Accept", "application/json")
      .basic_auth(&self.apikey, Some(&self.serverkey))
      .body(serde_json::to_vec(&payload)?)
      .send()
      .await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent Kumulos notification.");
      return Ok(true);
    }
    let reason = match status.as_u16() {
      401 => "Unauthorized - Invalid API and/or Server Key.",
      422 => "Unprocessable Entity - The request was unparsable.",
      400 => "Bad Request - Targeted users do not exist or have unsubscribed.",
      _ => status.canonical_reason().unwrap_or(""),
    };
    tracing::warn!("Failed to send Kumulos notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use base64::Engine;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const UUID4: &str = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";

  #[test]
  fn test_urls() {
    for key in ["w", "x", "y", "z"] {
      let url = format!("kumulos://{}/{}/", UUID4, key.repeat(36));
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
    assert!(from_url(&format!("kumulos://{}/{}+/", UUID4, "a".repeat(35))).is_some());
    let invalid = [
      "kumulos://".to_string(),
      "kumulos://:@/".to_string(),
      "kumulos://abcd/".to_string(),
      format!("kumulos://{}/", UUID4),
      format!("kumulos://{}/short/", UUID4),
      format!("kumulos://abcd/{}/", "w".repeat(36)),
    ];
    for url in &invalid {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_send() {
    let serverkey = "w".repeat(36);
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", UUID4, serverkey)));
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/v2/notifications"))
      .and(header("authorization", auth.as_str()))
      .and(header("accept", "application/json"))
      .and(body_json(json!({"target": {"broadcast": true}, "content": {"title": "T", "message": "B"}})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("kumulos://{}/{}/", UUID4, serverkey)).unwrap();
    assert_eq!((svc.title_maxlen(), svc.body_maxlen()), (64, 240));
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [400, 401, 422, 500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/v2/notifications")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("kumulos://{}/{}/", UUID4, "w".repeat(36))).unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Kumulos::static_details();
    assert_eq!(details.service_name, "Kumulos");
    assert_eq!(details.protocols, vec!["kumulos"]);
  }

  /// A YAML `apikey:` replaces the URL's, as upstream's constructor
  /// argument does (upstream: authenticates as 11111111-2222-4333-8444-...
  /// with the URL's server key).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_apikey_replaces_the_urls() {
    let (other, serverkey) = ("11111111-2222-4333-8444-555555555555", "w".repeat(36));
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{other}:{serverkey}")));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/notifications"))
      .and(header("authorization", auth.as_str()))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let doc = format!("urls:\n  - kumulos://{UUID4}/{serverkey}:\n      apikey: {other}\n");
    let (a, _) = crate::config::yaml::load_for_test(&doc).await;
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
  }
}
