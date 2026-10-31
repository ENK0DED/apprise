use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

/// Flowtriq DDoS alerting webhooks.
///
/// `flowtriq://apikey@host[:port]/webhook/path/` (HTTP) or `flowtriqs://...`
/// (HTTPS). The webhook path is the path the Flowtriq dashboard gives you,
/// kept exactly as written (percent-escapes included).
pub struct Flowtriq {
  apikey: String,
  host: String,
  port: Option<u16>,
  webhook_path: String,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Flowtriq {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.verified_host()?;
    // validate_regex(): must start with a non-space character; surrounding
    // whitespace is then stripped
    let apikey = url.user.as_deref().filter(|k| k.chars().next().is_some_and(|c| !c.is_whitespace()))?.trim().to_string();
    let webhook_path = url.path.trim_matches('/').to_string();
    if webhook_path.is_empty() {
      return None;
    }
    Some(Self { apikey, host, port: url.port, webhook_path, secure: url.schema == "flowtriqs", verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Flowtriq",
      service_url: Some("https://flowtriq.com"),
      setup_url: Some("https://appriseit.com/services/flowtriq/"),
      protocols: vec!["flowtriq", "flowtriqs"],
      description: "Forward alerts to a Flowtriq webhook channel.",
      attachment_support: false,
    }
  }

  fn severity(notify_type: &NotifyType) -> &'static str {
    match notify_type {
      NotifyType::Info => "info",
      NotifyType::Success => "success",
      NotifyType::Warning => "warning",
      NotifyType::Failure => "critical",
    }
  }
}

#[async_trait]
impl Notify for Flowtriq {
  fn schemas(&self) -> &[&str] {
    &["flowtriq", "flowtriqs"]
  }
  fn service_name(&self) -> &str {
    "Flowtriq"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Upstream disables throttling for this service
    0.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/{}", schema, self.host, port, self.webhook_path);

    let payload = json!({
      "title": ctx.title,
      "body": ctx.body,
      "severity": Self::severity(&ctx.notify_type),
      "type": ctx.notify_type.as_str(),
      "source": "apprise",
    });

    tracing::debug!("Flowtriq POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("X-API-Key", &self.apikey)
      .body(payload.to_string())
      .send()
      .await?;

    let status = resp.status().as_u16();
    if matches!(status, 200 | 201 | 202 | 204) {
      tracing::info!("Sent Flowtriq notification.");
      Ok(true)
    } else {
      let body = resp.text().await.unwrap_or_default();
      tracing::warn!("Failed to send Flowtriq notification: error={}.", status);
      Err(NotifyError::ServiceError { status, body })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Flowtriq {
    Flowtriq::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "flowtriq://myapikey@hostname/hooks/abc123/",
      "flowtriqs://myapikey@hostname/hooks/abc123/",
      "flowtriq://myapikey@hostname:8080/hooks/abc123/",
      "flowtriqs://myapikey@hostname:8443/hooks/abc123/",
      "flowtriqs://myapikey@flowtriq.com/api/v1/webhook/xyz/",
      "flowtriq://mykey@flowtriq.com/hooks/abc123",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "flowtriq://",
      "flowtriq://:@/",
      // No webhook path
      "flowtriq://apikey@hostname",
      "flowtriq://apikey@hostname///",
      // No API key
      "flowtriq://hostname/hooks/abc123",
      // Whitespace API key
      "flowtriq://%20%20@hostname/hooks/abc123",
      "flowtriq://%20key@hostname/hooks/abc123",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_verifies_host() {
    // Upstream parses this URL with host verification
    crate::notify::custom_json::assert_verifies_host("flowtriq://myapikey@{h}/hooks/abc123");
  }

  #[test]
  fn test_from_url_fields() {
    let f = parse("flowtriqs://my%20key@host:8443/hooks/a%2Fb/c%20d/");
    assert_eq!(f.apikey, "my key");
    assert_eq!(f.host, "host");
    assert_eq!(f.port, Some(8443));
    assert!(f.secure);
    // Escapes in the path are kept intact
    assert_eq!(f.webhook_path, "hooks/a%2Fb/c%20d");

    let f = parse("flowtriq://key@h/hooks/x");
    assert!(!f.secure);
    assert_eq!(f.port, None);
    assert_eq!(f.webhook_path, "hooks/x");
  }

  #[test]
  fn test_severity_map() {
    assert_eq!(Flowtriq::severity(&NotifyType::Info), "info");
    assert_eq!(Flowtriq::severity(&NotifyType::Success), "success");
    assert_eq!(Flowtriq::severity(&NotifyType::Warning), "warning");
    assert_eq!(Flowtriq::severity(&NotifyType::Failure), "critical");
  }

  #[tokio::test]
  async fn test_send_payload_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/hooks/inbound"))
      .and(header("X-API-Key", "apikey123"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({
        "title": "apprise parity",
        "body": "hello from python",
        "severity": "critical",
        "type": "failure",
        "source": "apprise",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let svc = from_url(&format!("flowtriq://apikey123@{}:{}/hooks/inbound/", addr.ip(), addr.port())).unwrap();
    let ctx = NotifyContext { title: "apprise parity".into(), body: "hello from python".into(), notify_type: NotifyType::Failure, ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_accepted_codes() {
    for code in [201u16, 202, 204] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/hooks/abc")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let addr = server.address();
      let svc = from_url(&format!("flowtriq://key@{}:{}/hooks/abc", addr.ip(), addr.port())).unwrap();
      assert!(svc.send(&NotifyContext { body: "body".into(), ..Default::default() }).await.unwrap(), "code {}", code);
    }
  }

  #[tokio::test]
  async fn test_send_failure_codes() {
    // Only 200/201/202/204 count as success
    for code in [500u16, 203, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/hooks/abc")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let addr = server.address();
      let svc = from_url(&format!("flowtriq://key@{}:{}/hooks/abc", addr.ip(), addr.port())).unwrap();
      assert!(svc.send(&NotifyContext { body: "body".into(), ..Default::default() }).await.is_err(), "code {}", code);
    }
  }

  #[test]
  fn test_service_details() {
    let details = Flowtriq::static_details();
    assert_eq!(details.service_name, "Flowtriq");
    assert_eq!(details.protocols, vec!["flowtriq", "flowtriqs"]);
    assert!(!details.attachment_support);
  }
}
