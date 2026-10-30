use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::{ParsedUrl, unquote};

/// Note visibilities; the first is the default.
const MISSKEY_VISIBILITIES: [&str; 4] = ["public", "home", "followers", "specified"];

pub struct Misskey {
  host: String,
  port: Option<u16>,
  token: String,
  visibility: &'static str,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Misskey {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;

    // ?token= wins; otherwise the user, when no password accompanies it
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => Some(unquote(t)),
      None if url.password.is_none() => url.user.clone(),
      None => None,
    };
    let Some(token) = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
      tracing::warn!("An invalid Misskey Access Token was specified.");
      return None;
    };

    // Visibility is matched by prefix (f -> followers), as upstream does
    let visibility = match url.get("visibility").filter(|v| !v.is_empty()) {
      Some(v) => {
        let vis = unquote(v).trim().to_lowercase();
        match MISSKEY_VISIBILITIES.iter().copied().find(|candidate| candidate.starts_with(vis.as_str())) {
          Some(found) => found,
          None => {
            tracing::warn!("The Misskey visibility specified ({}) is invalid.", v);
            return None;
          }
        }
      }
      None => MISSKEY_VISIBILITIES[0],
    };

    Some(Self { host, port: url.port, token, visibility, secure: url.schema == "misskeys", verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Misskey",
      service_url: Some("https://misskey-hub.net/"),
      setup_url: Some("https://appriseit.com/services/misskey/"),
      protocols: vec!["misskey", "misskeys"],
      description: "Post notes to Misskey instances.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Misskey {
  fn schemas(&self) -> &[&str] {
    &["misskey", "misskeys"]
  }
  fn service_name(&self) -> &str {
    "Misskey"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    512
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/api/notes/create", schema, self.host, port_str);
    let payload = json!({ "i": self.token, "text": ctx.body, "visibility": self.visibility });
    tracing::debug!("Misskey POST URL: {} (cert_verify={})", url, self.verify_certificate);

    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string()).send().await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent Misskey notification.");
      Ok(true)
    } else {
      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send Misskey notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::NotifyContext;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "misskey://access_token@hostname",
      "misskeys://access_token@hostname",
      "misskey://hostname/?token=abcd123",
      "misskeys://access_token@hostname:8443",
      "misskeys://access_token@hostname?visibility=specified",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["misskey://", "misskey://:@/", "misskey://hostname", "misskey://access_token@hostname?visibility=invalid"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn misskey_for_mock(server: &MockServer, token: &str) -> Misskey {
    let addr = server.address();
    let url_str = format!("misskey://{}@{}:{}", token, addr.ip(), addr.port());
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    Misskey::from_url(&parsed).unwrap()
  }

  #[tokio::test]
  async fn test_send_basic_success() {
    let server = MockServer::start().await;

    Mock::given(method("POST")).and(path("/api/notes/create")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let mk = misskey_for_mock(&server, "my_access_token");
    let ctx = NotifyContext { body: "Test Body".into(), ..Default::default() };
    let result = mk.send(&ctx).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_with_title_and_body() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/api/notes/create"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "i": "my_token",
          "text": "My Body",
          "visibility": "public",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let mk = misskey_for_mock(&server, "my_token");
    let ctx = NotifyContext { title: "My Title".into(), body: "My Body".into(), ..Default::default() };
    let result = mk.send(&ctx).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_body_only_no_title() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/api/notes/create"))
      .and(wiremock::matchers::body_json(serde_json::json!({
          "i": "tok123",
          "text": "Just a body",
          "visibility": "public",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let mk = misskey_for_mock(&server, "tok123");
    let ctx = NotifyContext { body: "Just a body".into(), ..Default::default() };
    let result = mk.send(&ctx).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_token_in_payload() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/api/notes/create"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({
          "i": "secret_token_abc",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let mk = misskey_for_mock(&server, "secret_token_abc");
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    let result = mk.send(&ctx).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_send_server_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST")).and(path("/api/notes/create")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;

    let mk = misskey_for_mock(&server, "my_token");
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    let result = mk.send(&ctx).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_send_non_standard_error_code() {
    let server = MockServer::start().await;

    // Use 418 (I'm a teapot) as a non-standard error
    Mock::given(method("POST")).and(path("/api/notes/create")).respond_with(ResponseTemplate::new(418)).expect(1).mount(&server).await;

    let mk = misskey_for_mock(&server, "my_token");
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    let result = mk.send(&ctx).await;
    assert!(result.is_err());
  }

  #[test]
  fn test_visibility_settings() {
    // Valid visibility values
    for vis in &["public", "home", "followers", "specified"] {
      let url = format!("misskey://token@host?visibility={}", vis);
      assert!(from_url(&url).is_some(), "Should accept visibility={}", vis);
    }

    // Invalid visibility
    let url = "misskey://token@host?visibility=invalid";
    assert!(from_url(url).is_none());
  }

  #[test]
  fn test_token_and_visibility_parsing() {
    let parse = |u: &str| Misskey::from_url(&crate::utils::parse::ParsedUrl::parse(u).unwrap());
    // ?token= wins over the user
    assert_eq!(parse("misskey://user@hostname/?token=abcd123").unwrap().token, "abcd123");
    assert_eq!(parse("misskey://hostname/?token=abcd123").unwrap().token, "abcd123");
    // A user with a password is not a token
    assert!(parse("misskey://user:pass@hostname").is_none());
    // Visibility matches by prefix and ignores case
    assert_eq!(parse("misskey://t@h?visibility=FOL").unwrap().visibility, "followers");
    assert_eq!(parse("misskey://t@h?visibility=s").unwrap().visibility, "specified");
    assert_eq!(parse("misskey://t@h").unwrap().visibility, "public");
  }

  #[tokio::test]
  async fn test_send_visibility_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/notes/create"))
      .and(wiremock::matchers::header("Content-Type", "application/json"))
      .and(wiremock::matchers::body_json(serde_json::json!({"i": "tok", "text": "b", "visibility": "home"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("misskey://tok@{}:{}?visibility=home", addr.ip(), addr.port())).unwrap();
    let mk = Misskey::from_url(&parsed).unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(mk.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_non_200_success_code_fails() {
    let server = MockServer::start().await;
    // Upstream only accepts 200
    Mock::given(method("POST")).and(path("/api/notes/create")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    let mk = misskey_for_mock(&server, "my_token");
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    assert!(mk.send(&ctx).await.is_err());
  }

  #[test]
  fn test_secure_vs_insecure() {
    let parsed = crate::utils::parse::ParsedUrl::parse("misskey://token@host").unwrap();
    let mk = Misskey::from_url(&parsed).unwrap();
    assert!(!mk.secure);

    let parsed = crate::utils::parse::ParsedUrl::parse("misskeys://token@host").unwrap();
    let mk = Misskey::from_url(&parsed).unwrap();
    assert!(mk.secure);
  }

  #[test]
  fn test_custom_port() {
    let parsed = crate::utils::parse::ParsedUrl::parse("misskeys://token@host:8443").unwrap();
    let mk = Misskey::from_url(&parsed).unwrap();
    assert_eq!(mk.port, Some(8443));
  }
}
