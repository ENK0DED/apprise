use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;

pub struct Gotify {
  host: String,
  port: Option<u16>,
  token: String,
  path: String,
  secure: bool,
  priority: i32,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Gotify {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // gotify://host/token  or  gotifys://host/token
    // gotify://host:port/path/token
    let host = url.host.clone()?;
    let token = url.path_parts.last()?.clone();
    if token.is_empty() {
      return None;
    }

    // Path prefix is everything except the last component
    let path = if url.path_parts.len() > 1 { format!("/{}/", url.path_parts[..url.path_parts.len() - 1].join("/")) } else { "/".to_string() };

    let priority = url
      .get("priority")
      .and_then(|p| match p.to_lowercase().as_str() {
        "l" | "low" | "1" => Some(1),
        "m" | "moderate" | "3" => Some(3),
        "n" | "normal" | "5" => Some(5),
        "h" | "high" | "8" => Some(8),
        "e" | "emergency" | "10" => Some(10),
        n => n.parse().ok(),
      })
      .unwrap_or(5);

    Some(Self {
      host,
      port: url.port,
      token,
      path,
      secure: url.schema.ends_with('s'),
      priority,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Gotify",
      service_url: Some("https://gotify.net"),
      setup_url: Some("https://gotify.net/docs/pushmsg"),
      protocols: vec!["gotify", "gotifys"],
      description: "Send notifications via Gotify self-hosted push server.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Gotify {
  fn schemas(&self) -> &[&str] {
    &["gotify", "gotifys"]
  }
  fn service_name(&self) -> &str {
    "Gotify"
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
    let api_url = format!("{}://{}{}{}message", schema, self.host, port_str, self.path);

    let mut payload = json!({
        "title": ctx.title,
        "message": ctx.body,
        "priority": self.priority,
    });

    // Include markdown extras only when format is Markdown (matching Python)
    if ctx.body_format == crate::types::NotifyFormat::Markdown {
      payload["extras"] = json!({
          "client::display": { "contentType": "text/markdown" }
      });
    }

    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&api_url).header("User-Agent", APP_ID).header("X-Gotify-Key", &self.token).json(&payload).send().await?;

    if resp.status().is_success() {
      tracing::info!("Gotify notification sent");
      Ok(true)
    } else {
      let status = resp.status().as_u16();
      let body = resp.text().await.unwrap_or_default();
      Err(NotifyError::ServiceError { status, body })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::NotifyContext;
  use crate::notify::registry::from_url;
  use crate::types::NotifyFormat;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["gotify://", "gotify://hostname", "gotify://:@/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_url_parsing() {
    let token = "t".repeat(16);

    // Basic hostname + token
    let url = format!("gotify://hostname/{}", token);
    let notifier = from_url(&url);
    assert!(notifier.is_some(), "Should parse: {}", url);

    // With port
    let url = format!("gotify://hostname:8008/{}", token);
    assert!(from_url(&url).is_some());

    // Secure variant
    let url = format!("gotifys://hostname/{}", token);
    assert!(from_url(&url).is_some());

    // With path prefix
    let url = format!("gotify://hostname/a/path/{}", token);
    assert!(from_url(&url).is_some());
  }

  #[test]
  fn test_priority_mapping() {
    let token = "a".repeat(16);
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}?priority=low", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 1);

    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}?priority=moderate", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 3);

    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}?priority=normal", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 5);

    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}?priority=high", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 8);

    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}?priority=emergency", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 10);

    // Numeric priority
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}?priority=7", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 7);

    // Invalid priority defaults to normal (5)
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host:8008/{}?priority=invalid", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 5);

    // No priority defaults to normal (5)
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.priority, 5);
  }

  #[test]
  fn test_secure_vs_insecure() {
    let token = "a".repeat(16);

    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert!(!g.secure);

    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotifys://host/{}", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert!(g.secure);
  }

  #[test]
  fn test_path_construction() {
    let token = "a".repeat(16);

    // No path prefix -> "/"
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/{}", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.path, "/");

    // With path prefix
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("gotify://host/a/path/ending/in/a/slash/{}", token)).unwrap();
    let g = Gotify::from_url(&parsed).unwrap();
    assert_eq!(g.path, "/a/path/ending/in/a/slash/");
  }

  /// Helper: create a Gotify instance pointing at the given mock server.
  fn gotify_for_mock(server: &MockServer, token: &str) -> Gotify {
    let addr = server.address();
    let url_str = format!("gotify://{}:{}/{}", addr.ip(), addr.port(), token);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    Gotify::from_url(&parsed).unwrap()
  }

  fn default_ctx() -> NotifyContext {
    NotifyContext { title: "Test Title".into(), body: "Test Body".into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_send_basic_success() {
    let server = MockServer::start().await;
    let token = "t".repeat(16);

    Mock::given(method("POST"))
      .and(path("/message"))
      .and(header("X-Gotify-Key", token.as_str()))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let gotify = gotify_for_mock(&server, &token);
    let ctx = default_ctx();
    let result = gotify.send(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[tokio::test]
  async fn test_send_verifies_json_payload() {
    let server = MockServer::start().await;
    let token = "a".repeat(16);

    Mock::given(method("POST"))
      .and(path("/message"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "title": "My Title",
          "message": "My Body",
          "priority": 5,
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let gotify = gotify_for_mock(&server, &token);
    let ctx = NotifyContext { title: "My Title".into(), body: "My Body".into(), ..Default::default() };
    let result = gotify.send(&ctx).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_with_priority() {
    let server = MockServer::start().await;
    let token = "b".repeat(16);

    // Create a Gotify with high priority
    let addr = server.address();
    let url_str = format!("gotify://{}:{}/{}?priority=high", addr.ip(), addr.port(), token);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    let gotify = Gotify::from_url(&parsed).unwrap();

    Mock::given(method("POST"))
      .and(path("/message"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "title": "Test Title",
          "message": "Test Body",
          "priority": 8,
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let result = gotify.send(&default_ctx()).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_with_path_prefix() {
    let server = MockServer::start().await;
    let token = "c".repeat(16);

    let addr = server.address();
    let url_str = format!("gotify://{}:{}/custom/path/{}", addr.ip(), addr.port(), token);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    let gotify = Gotify::from_url(&parsed).unwrap();

    // The API URL should be /custom/path/message
    Mock::given(method("POST")).and(path("/custom/path/message")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let result = gotify.send(&default_ctx()).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_error_500() {
    let server = MockServer::start().await;
    let token = "d".repeat(16);

    Mock::given(method("POST"))
      .and(path("/message"))
      .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
      .expect(1)
      .mount(&server)
      .await;

    let gotify = gotify_for_mock(&server, &token);
    let result = gotify.send(&default_ctx()).await;
    assert!(result.is_err());
    match result.unwrap_err() {
      NotifyError::ServiceError { status, body } => {
        assert_eq!(status, 500);
        assert_eq!(body, "Internal Server Error");
      }
      other => panic!("Expected ServiceError, got: {:?}", other),
    }
  }

  #[tokio::test]
  async fn test_send_error_401_unauthorized() {
    let server = MockServer::start().await;
    let token = "e".repeat(16);

    Mock::given(method("POST")).and(path("/message")).respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized")).expect(1).mount(&server).await;

    let gotify = gotify_for_mock(&server, &token);
    let result = gotify.send(&default_ctx()).await;
    assert!(result.is_err());
    match result.unwrap_err() {
      NotifyError::ServiceError { status, .. } => {
        assert_eq!(status, 401);
      }
      other => panic!("Expected ServiceError, got: {:?}", other),
    }
  }

  #[tokio::test]
  async fn test_send_connection_failure() {
    // Point at a port that nothing is listening on
    let token = "f".repeat(16);
    let url_str = format!("gotify://127.0.0.1:1/{}", token);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    let gotify = Gotify::from_url(&parsed).unwrap();

    let result = gotify.send(&default_ctx()).await;
    assert!(result.is_err());
    // Should be an HTTP/connection error, not a ServiceError
    match result.unwrap_err() {
      NotifyError::Http(_) => {} // expected
      other => panic!("Expected Http error, got: {:?}", other),
    }
  }

  #[tokio::test]
  async fn test_send_bizarre_status_code() {
    // Matching the Python test that uses status code 999
    // wiremock doesn't support 999 (not a valid HTTP status), so use 418 as
    // a non-standard error
    let server = MockServer::start().await;
    let token = "g".repeat(16);

    Mock::given(method("POST")).and(path("/message")).respond_with(ResponseTemplate::new(418)).expect(1).mount(&server).await;

    let gotify = gotify_for_mock(&server, &token);
    let result = gotify.send(&default_ctx()).await;
    assert!(result.is_err());
    match result.unwrap_err() {
      NotifyError::ServiceError { status, .. } => {
        assert_eq!(status, 418);
      }
      other => panic!("Expected ServiceError, got: {:?}", other),
    }
  }

  #[tokio::test]
  async fn test_send_includes_user_agent() {
    use crate::notify::APP_ID;

    let server = MockServer::start().await;
    let token = "h".repeat(16);

    Mock::given(method("POST")).and(path("/message")).and(header("User-Agent", APP_ID)).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let gotify = gotify_for_mock(&server, &token);
    let result = gotify.send(&default_ctx()).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_markdown_includes_extras() {
    let server = MockServer::start().await;
    let token = "i".repeat(16);

    Mock::given(method("POST"))
      .and(path("/message"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "title": "MD Title",
          "message": "**bold**",
          "priority": 5,
          "extras": {
              "client::display": { "contentType": "text/markdown" }
          }
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let gotify = gotify_for_mock(&server, &token);
    let ctx = NotifyContext { title: "MD Title".into(), body: "**bold**".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    let result = gotify.send(&ctx).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_text_no_extras() {
    let server = MockServer::start().await;
    let token = "j".repeat(16);

    // Text format should NOT include extras
    Mock::given(method("POST"))
      .and(path("/message"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "title": "Text Title",
          "message": "plain text",
          "priority": 5,
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let gotify = gotify_for_mock(&server, &token);
    let ctx = NotifyContext { title: "Text Title".into(), body: "plain text".into(), body_format: NotifyFormat::Text, ..Default::default() };
    let result = gotify.send(&ctx).await;
    assert!(result.is_ok());
  }
}
