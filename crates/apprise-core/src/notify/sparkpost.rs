use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;

pub struct SparkPost {
  api_key: String,
  from: String,
  targets: Vec<String>,
  pub(crate) host: String,
  pub(crate) scheme: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl SparkPost {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Require user@ for identity
    let user = url.user.clone()?;
    if user.is_empty() {
      return None;
    }
    // Reject quotes in user
    if user.contains('"') {
      return None;
    }
    let api_key = url.host.clone()?;
    let from = url.get("from").unwrap_or("apprise@sparkpost.com").to_string();
    let targets: Vec<String> = url.path_parts.iter().map(|s| if s.contains('@') { s.clone() } else { format!("{}@sparkpost.com", s) }).collect();
    if targets.is_empty() {
      return None;
    }
    // Validate region if provided
    if let Some(region) = url.get("region") {
      match region.to_lowercase().as_str() {
        "us" | "eu" | "" => {}
        _ => return None,
      }
    }
    let host = "api.sparkpost.com".to_string();
    Some(Self { api_key, from, targets, host, scheme: "https".to_string(), verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SparkPost",
      service_url: Some("https://www.sparkpost.com"),
      setup_url: None,
      protocols: vec!["sparkpost"],
      description: "Send email via SparkPost.",
      attachment_support: true,
    }
  }
}
#[async_trait]
impl Notify for SparkPost {
  fn schemas(&self) -> &[&str] {
    &["sparkpost"]
  }
  fn service_name(&self) -> &str {
    "SparkPost"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let recipients: Vec<_> = self.targets.iter().map(|t| json!({ "address": { "email": t } })).collect();
    let mut content = json!({ "from": self.from, "subject": ctx.title, "text": ctx.body });
    if !ctx.attachments.is_empty() {
      content["attachments"] = json!(
        ctx
          .attachments
          .iter()
          .map(|att| json!({
              "name": att.name,
              "type": att.mime_type,
              "data": base64::engine::general_purpose::STANDARD.encode(&att.data),
          }))
          .collect::<Vec<_>>()
      );
    }
    let payload = json!({ "recipients": recipients, "content": content });
    let url = format!("{}://{}/api/v1/transmissions", self.scheme, self.host);
    let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", &self.api_key).json(&payload).send().await?;
    Ok(resp.status().is_success())
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
  fn test_invalid_urls() {
    let no_user = format!("sparkpost://localhost.localdomain/{}", "a".repeat(32));
    let bad_email = format!("sparkpost://\"@localhost.localdomain/{}", "b".repeat(32));
    let bad_region = format!("sparkpost://user@localhost.localdomain/{}?region=invalid", "a".repeat(32));
    let urls: Vec<&str> = vec!["sparkpost://", "sparkpost://:@/", "sparkpost://user@localhost.localdomain", &no_user, &bad_email, &bad_region];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("sparkpost://user@localhost.localdomain/{}", "c".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?format=markdown", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?format=html", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?format=text", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?region=uS", "d".repeat(32)),
      format!("sparkpost://user@localhost.localdomain/{}?region=EU", "e".repeat(32)),
      // headers
      format!("sparkpost://user@localhost.localdomain/{}?+X-Customer-Campaign-ID=Apprise", "f".repeat(32)),
      // bcc and cc
      format!("sparkpost://user@localhost.localdomain/{}?bcc=user@example.com&cc=user2@example.com", "h".repeat(32)),
      // One To email
      format!("sparkpost://user@localhost.localdomain/{}/test@example.com", "a".repeat(32)),
      // To via query
      format!("sparkpost://user@localhost.localdomain/{}?to=test@example.com", "k".repeat(32)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("sparkpost://user@myapikey/recipient@example.com").unwrap();
    let sp = SparkPost::from_url(&parsed).unwrap();
    assert_eq!(sp.api_key, "myapikey");
    assert_eq!(sp.host, "api.sparkpost.com");
    assert!(sp.targets.contains(&"recipient@example.com".to_string()));
  }

  #[test]
  fn test_from_url_non_email_target() {
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("sparkpost://user@apikey/{}/invalid", "a".repeat(32))).unwrap();
    let sp = SparkPost::from_url(&parsed).unwrap();
    // non-email targets get @sparkpost.com appended
    assert!(sp.targets.iter().any(|t| t.ends_with("@sparkpost.com")));
  }

  #[test]
  fn test_region_validation() {
    // US region valid
    let parsed = crate::utils::parse::ParsedUrl::parse("sparkpost://user@apikey/target?region=us").unwrap();
    assert!(SparkPost::from_url(&parsed).is_some());

    // EU region valid
    let parsed = crate::utils::parse::ParsedUrl::parse("sparkpost://user@apikey/target?region=EU").unwrap();
    assert!(SparkPost::from_url(&parsed).is_some());

    // Invalid region
    let parsed = crate::utils::parse::ParsedUrl::parse("sparkpost://user@apikey/target?region=invalid").unwrap();
    assert!(SparkPost::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = SparkPost::static_details();
    assert_eq!(details.service_name, "SparkPost");
    assert_eq!(details.service_url, Some("https://www.sparkpost.com"));
    assert!(details.protocols.contains(&"sparkpost"));
    assert!(details.attachment_support);
  }

  /// Helper: create a SparkPost instance pointing at the given mock server.
  fn sparkpost_for_mock(server: &MockServer) -> SparkPost {
    let addr = server.address();
    let parsed = crate::utils::parse::ParsedUrl::parse("sparkpost://user@apikey/recipient@example.com").unwrap();
    let mut sp = SparkPost::from_url(&parsed).unwrap();
    sp.host = format!("{}:{}", addr.ip(), addr.port());
    sp.scheme = "http".to_string();
    sp
  }

  fn default_ctx() -> NotifyContext {
    NotifyContext { title: "Test Title".into(), body: "Test Body".into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_send_success() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/api/v1/transmissions"))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
          "results": {
              "total_rejected_recipients": 0,
              "total_accepted_recipients": 1,
              "id": "11668787484950529",
          }
      })))
      .expect(1)
      .mount(&server)
      .await;

    let sp = sparkpost_for_mock(&server);
    let result = sp.send(&default_ctx()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[tokio::test]
  async fn test_send_server_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST")).and(path("/api/v1/transmissions")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;

    let sp = sparkpost_for_mock(&server);
    let result = sp.send(&default_ctx()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), false);
  }

  #[tokio::test]
  async fn test_send_multiple_recipients() {
    let server = MockServer::start().await;

    Mock::given(method("POST")).and(path("/api/v1/transmissions")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let addr = server.address();
    let parsed = crate::utils::parse::ParsedUrl::parse("sparkpost://user@apikey/user1@example.com/user2@example.com").unwrap();
    let mut sp = SparkPost::from_url(&parsed).unwrap();
    sp.host = format!("{}:{}", addr.ip(), addr.port());
    sp.scheme = "http".to_string();

    let result = sp.send(&default_ctx()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[tokio::test]
  async fn test_send_verifies_json_payload() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/api/v1/transmissions"))
      .and(wiremock::matchers::body_partial_json(serde_json::json!({
          "content": {
              "from": "apprise@sparkpost.com",
              "subject": "Test Title",
              "text": "Test Body",
          },
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let sp = sparkpost_for_mock(&server);
    let result = sp.send(&default_ctx()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[tokio::test]
  async fn test_send_includes_authorization_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/api/v1/transmissions"))
      .and(wiremock::matchers::header("Authorization", "apikey"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let sp = sparkpost_for_mock(&server);
    let result = sp.send(&default_ctx()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }
}
