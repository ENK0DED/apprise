use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct HomeAssistant {
  host: String,
  port: Option<u16>,
  access_token: String,
  secure: bool,
  notification_id: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl HomeAssistant {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // hassio://access_token@host[:port]
    // hassio://host/access_token
    // hassio://host/path?accesstoken=token
    let host = url.host.clone()?;
    if host.is_empty() {
      return None;
    }
    let secure = url.schema == "hassios";

    // Try to get access token from:
    // 1. ?accesstoken= query param
    // 2. Last path part
    // 3. user field (when path_parts also present - user is just auth, token is in path)
    let access_token = url.get("accesstoken").map(|s| s.to_string()).or_else(|| url.path_parts.last().cloned()).or_else(|| {
      // Only use user/password as token if path_parts has content
      if !url.path_parts.is_empty() { url.user.clone().or_else(|| url.password.clone()) } else { None }
    })?;

    if access_token.trim().is_empty() {
      return None;
    }

    // Validate notification ID if provided
    let notification_id = match url.get("nid").or_else(|| url.get("id")) {
      Some(nid) => {
        let nid = nid.to_string();
        // Reject invalid chars in notification ID
        if nid.contains('!') || nid.contains('%') {
          return None;
        }
        Some(nid)
      }
      None => None,
    };

    // Default insecure port is 8123 (matching Python)
    let port = url.port.or(if secure { None } else { Some(8123) });
    Some(Self { host, port, access_token, secure, notification_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Home Assistant",
      service_url: Some("https://www.home-assistant.io"),
      setup_url: None,
      protocols: vec!["hassio", "hassios"],
      description: "Send via Home Assistant persistent notifications.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for HomeAssistant {
  fn schemas(&self) -> &[&str] {
    &["hassio", "hassios"]
  }
  fn service_name(&self) -> &str {
    "Home Assistant"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/api/services/persistent_notification/create", schema, self.host, port_str);
    let mut payload = json!({ "title": ctx.title, "message": ctx.body });
    if let Some(ref id) = self.notification_id {
      payload["notification_id"] = json!(id);
    }
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.access_token)).json(&payload).send().await?;
    if resp.status().is_success() {
      Ok(true)
    } else {
      Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "hassio://localhost/long-lived-access-token",
      "hassio://user:pass@localhost/long-lived-access-token/",
      "hassio://localhost:80/long-lived-access-token",
      "hassio://user@localhost:8123/llat",
      "hassios://localhost/llat?nid=abcd",
      "hassios://user:pass@localhost/llat",
      "hassios://localhost:8443/path/llat/",
      "hassio://localhost:8123/a/path?accesstoken=llat",
      "hassios://user:password@localhost:80/llat/",
      "hassio://user:pass@localhost/llat",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["hassio://:@/", "hassio://", "hassios://", "hassio://user@localhost", "hassios://localhost/llat?nid=!%"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  // ── Behavioral tests using wiremock ──────────────────────────────────

  use super::*;
  use crate::asset::AppriseAsset;
  use crate::notify::{Notify, NotifyContext};
  use crate::types::{NotifyFormat, NotifyType};
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// Helper: build a NotifyContext with sensible defaults.
  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext {
      body: body.to_string(),
      title: title.to_string(),
      notify_type: NotifyType::Info,
      body_format: NotifyFormat::Text,
      attachments: vec![],
      interpret_escapes: false,
      interpret_emojis: false,
      tags: vec![],
      asset: AppriseAsset::default(),
    }
  }

  /// Helper: create a HomeAssistant instance pointing at the mock server.
  fn ha_for_mock(server: &MockServer, token: &str) -> HomeAssistant {
    let addr = server.address();
    let port = addr.port();
    let url_str = format!("hassio://localhost:{}/{}", port, token);
    let parsed = ParsedUrl::parse(&url_str).expect("parse test URL");
    HomeAssistant::from_url(&parsed).expect("create HomeAssistant from test URL")
  }

  // ── 1. Basic POST with correct JSON payload ─────────────────────────

  #[tokio::test]
  async fn test_basic_send_posts_to_correct_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(header("Authorization", "Bearer accesstoken"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let ha = ha_for_mock(&server, "accesstoken");
    let result = ha.send(&ctx("hello", "world")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_includes_title_and_body_in_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "title": "My Title",
          "message": "My Body"
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let ha = ha_for_mock(&server, "mytoken");
    let result = ha.send(&ctx("My Title", "My Body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 2. Authorization header with long-lived access token ────────────

  #[tokio::test]
  async fn test_bearer_token_in_auth_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(header("Authorization", "Bearer long-lived-access-token"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let ha = ha_for_mock(&server, "long-lived-access-token");
    let result = ha.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_accesstoken_from_query_param() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(header("Authorization", "Bearer llat"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("hassio://localhost:{}/a/path?accesstoken=llat", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();

    let result = ha.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 3. Custom port and path ─────────────────────────────────────────

  #[tokio::test]
  async fn test_custom_port() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("hassio://localhost:{}/mytoken", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();

    let result = ha.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_default_port_8123_for_insecure() {
    // hassio:// without explicit port should default to 8123
    let parsed = ParsedUrl::parse("hassio://localhost/mytoken").unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();
    assert_eq!(ha.port, Some(8123));
  }

  #[tokio::test]
  async fn test_secure_no_default_port() {
    // hassios:// without explicit port should have no port set
    let parsed = ParsedUrl::parse("hassios://localhost/mytoken").unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();
    assert_eq!(ha.port, None);
  }

  // ── 4. Notification ID ──────────────────────────────────────────────

  #[tokio::test]
  async fn test_notification_id_included_in_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "title": "t",
          "message": "b",
          "notification_id": "abcd"
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("hassio://localhost:{}/mytoken?nid=abcd", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();

    let result = ha.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 5. Error handling ───────────────────────────────────────────────

  #[tokio::test]
  async fn test_http_500_returns_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;

    let ha = ha_for_mock(&server, "mytoken");
    let result = ha.send(&ctx("title", "body")).await;
    assert!(result.is_err(), "HTTP 500 should return Err");
  }

  #[tokio::test]
  async fn test_http_401_returns_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .respond_with(ResponseTemplate::new(401))
      .expect(1)
      .mount(&server)
      .await;

    let ha = ha_for_mock(&server, "badtoken");
    let result = ha.send(&ctx("title", "body")).await;
    assert!(result.is_err(), "HTTP 401 should return Err");
  }

  #[tokio::test]
  async fn test_http_error_contains_status_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
      .expect(1)
      .mount(&server)
      .await;

    let ha = ha_for_mock(&server, "mytoken");
    let result = ha.send(&ctx("title", "body")).await;
    match result {
      Err(crate::error::NotifyError::ServiceError { status, body }) => {
        assert_eq!(status, 500);
        assert_eq!(body, "server error");
      }
      other => panic!("Expected ServiceError, got {:?}", other),
    }
  }

  #[tokio::test]
  async fn test_connection_refused_returns_error() {
    // Point at a port that nothing is listening on
    let parsed = ParsedUrl::parse("hassio://localhost:19999/mytoken").unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();

    let result = ha.send(&ctx("title", "body")).await;
    assert!(result.is_err(), "Connection refused should return Err");
  }

  // ── 6. Secure vs insecure mode ──────────────────────────────────────

  #[test]
  fn test_hassios_sets_secure_flag() {
    let parsed = ParsedUrl::parse("hassios://localhost/mytoken").unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();
    assert!(ha.secure);
  }

  #[test]
  fn test_hassio_sets_insecure_flag() {
    let parsed = ParsedUrl::parse("hassio://localhost/mytoken").unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();
    assert!(!ha.secure);
  }

  // ── 7. User-Agent header ────────────────────────────────────────────

  #[tokio::test]
  async fn test_user_agent_header_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(header("User-Agent", crate::notify::APP_ID))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let ha = ha_for_mock(&server, "mytoken");
    let result = ha.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  // ── 8. User:pass in URL with access token in path ───────────────────

  #[tokio::test]
  async fn test_user_pass_url_with_path_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(header("Authorization", "Bearer long-lived-access-token"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let url_str = format!("hassio://user:pass@localhost:{}/long-lived-access-token/", addr.port());
    let parsed = ParsedUrl::parse(&url_str).unwrap();
    let ha = HomeAssistant::from_url(&parsed).unwrap();

    let result = ha.send(&ctx("t", "b")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }
}
