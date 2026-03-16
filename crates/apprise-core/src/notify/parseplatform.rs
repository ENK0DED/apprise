use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct ParsePlatform {
  host: String,
  port: Option<u16>,
  app_id: String,
  master_key: String,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl ParsePlatform {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;
    let app_id = url.user.clone().or_else(|| url.get("app_id").map(|s| s.to_string()))?;
    let master_key = url.password.clone().or_else(|| url.get("master_key").map(|s| s.to_string()))?;
    // Validate device param if provided
    if let Some(device) = url.get("device") {
      match device.to_lowercase().as_str() {
        "ios" | "android" | "" => {}
        _ => return None,
      }
    }
    Some(Self { host, port: url.port, app_id, master_key, secure: url.schema == "parseps", verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Parse Platform",
      service_url: Some("https://parseplatform.org"),
      setup_url: None,
      protocols: vec!["parsep", "parseps"],
      description: "Send push via Parse Platform.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for ParsePlatform {
  fn schemas(&self) -> &[&str] {
    &["parsep", "parseps"]
  }
  fn service_name(&self) -> &str {
    "Parse Platform"
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
    let url = format!("{}://{}{}/parse/push/", schema, self.host, port_str);
    let payload = json!({ "where": {}, "data": { "title": ctx.title, "alert": ctx.body } });
    let client = build_client(self.verify_certificate)?;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("X-Parse-Application-Id", self.app_id.as_str())
      .header("X-Parse-Master-Key", self.master_key.as_str())
      .json(&payload)
      .send()
      .await?;
    if resp.status().is_success() {
      Ok(true)
    } else {
      Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::NotifyContext;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let kwargs_url = format!("parseps://localhost?app_id={}&master_key={}", "a".repeat(32), "d".repeat(32));
    let urls: Vec<&str> = vec![
      "parsep://app_id:master_key@localhost:8080?device=ios",
      "parseps://app_id:master_key@localhost",
      &kwargs_url,
      "parsep://app_id:master_key@localhost:8080?device=android",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let no_master = format!("parsep://app_id@{}", "a".repeat(32));
    let no_appid = format!("parseps://:master_key@{}", "a".repeat(32));
    let urls: Vec<&str> = vec!["parsep://", "parsep://:@/", "parsep://app_id:master_key@localhost?device=invalid", &no_master, &no_appid];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_basic_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("parsep://app_id:master_key@localhost:8080").unwrap();
    let obj = ParsePlatform::from_url(&parsed).unwrap();
    assert_eq!(obj.host, "localhost");
    assert_eq!(obj.port, Some(8080));
    assert_eq!(obj.app_id, "app_id");
    assert_eq!(obj.master_key, "master_key");
    assert!(!obj.secure);
  }

  #[test]
  fn test_from_url_secure() {
    let parsed = crate::utils::parse::ParsedUrl::parse("parseps://app_id:master_key@localhost").unwrap();
    let obj = ParsePlatform::from_url(&parsed).unwrap();
    assert!(obj.secure);
  }

  #[test]
  fn test_from_url_kwargs() {
    let url = format!("parseps://localhost?app_id={}&master_key={}", "a".repeat(32), "d".repeat(32));
    let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
    let obj = ParsePlatform::from_url(&parsed).unwrap();
    assert_eq!(obj.app_id, "a".repeat(32));
    assert_eq!(obj.master_key, "d".repeat(32));
  }

  #[test]
  fn test_service_details() {
    let details = ParsePlatform::static_details();
    assert_eq!(details.service_name, "Parse Platform");
    assert!(details.protocols.contains(&"parsep"));
    assert!(details.protocols.contains(&"parseps"));
  }

  fn default_ctx() -> NotifyContext {
    NotifyContext { title: "Test Title".into(), body: "Test Body".into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_send_success() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/parse/push/"))
      .and(header("X-Parse-Application-Id", "myapp"))
      .and(header("X-Parse-Master-Key", "mykey"))
      .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let obj = ParsePlatform {
      host: addr.ip().to_string(),
      port: Some(addr.port()),
      app_id: "myapp".into(),
      master_key: "mykey".into(),
      secure: false,
      verify_certificate: false,
      tags: vec![],
    };

    let result = obj.send(&default_ctx()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[tokio::test]
  async fn test_send_server_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/parse/push/"))
      .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let obj = ParsePlatform {
      host: addr.ip().to_string(),
      port: Some(addr.port()),
      app_id: "myapp".into(),
      master_key: "mykey".into(),
      secure: false,
      verify_certificate: false,
      tags: vec![],
    };

    let result = obj.send(&default_ctx()).await;
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_send_verifies_headers() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
      .and(path("/parse/push/"))
      .and(header("X-Parse-Application-Id", "testapp"))
      .and(header("X-Parse-Master-Key", "testmaster"))
      .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
      .expect(1)
      .mount(&server)
      .await;

    let addr = server.address();
    let obj = ParsePlatform {
      host: addr.ip().to_string(),
      port: Some(addr.port()),
      app_id: "testapp".into(),
      master_key: "testmaster".into(),
      secure: false,
      verify_certificate: false,
      tags: vec![],
    };

    let result = obj.send(&default_ctx()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }
}
