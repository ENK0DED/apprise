use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

pub struct Enigma2 {
  host: String,
  port: u16,
  user: Option<String>,
  password: Option<String>,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Enigma2 {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;
    let secure = url.schema.ends_with('s');
    let port = url.port.unwrap_or(if secure { 443 } else { 80 });
    Some(Self { host, port, user: url.user.clone(), password: url.password.clone(), secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Enigma2",
      service_url: None,
      setup_url: None,
      protocols: vec!["enigma2", "enigma2s"],
      description: "Send notifications to Enigma2 receivers.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Enigma2 {
  fn schemas(&self) -> &[&str] {
    &["enigma2", "enigma2s"]
  }
  fn service_name(&self) -> &str {
    "Enigma2"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let schema = if self.secure { "https" } else { "http" };
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
    let url = format!("{}://{}:{}/web/message?text={}&type=1", schema, self.host, self.port, urlencoding::encode(&msg));
    let mut req = client.get(&url).header("User-Agent", APP_ID);
    if let (Some(u), Some(p)) = (&self.user, &self.password) {
      req = req.basic_auth(u, Some(p));
    }
    let resp = req.send().await?;
    Ok(resp.status().is_success())
  }
}

#[cfg(test)]
mod tests {
  use crate::notify::registry::from_url;
  use crate::notify::{Notify, NotifyContext};
  use wiremock::matchers::{method, path_regex};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "enigma2://localhost",
      "enigma2://user@localhost",
      "enigma2://user@localhost?timeout=-1",
      "enigma2://user@localhost?timeout=-1000",
      "enigma2://user@localhost?timeout=invalid",
      "enigma2://user:pass@localhost",
      "enigma2://localhost:8080",
      "enigma2://user:pass@localhost:8080",
      "enigma2s://localhost",
      "enigma2s://user:pass@localhost",
      "enigma2s://localhost:8080/path/",
      "enigma2s://user:pass@localhost:8080",
      "enigma2://localhost:8080/path?+HeaderKey=HeaderValue",
      "enigma2://user:pass@localhost:8083",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["enigma2://:@/", "enigma2://", "enigma2s://"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn enigma2_for_mock(server: &MockServer) -> super::Enigma2 {
    let addr = server.address();
    let url_str = format!("enigma2://{}:{}", addr.ip(), addr.port());
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    super::Enigma2::from_url(&parsed).unwrap()
  }

  fn enigma2_with_auth_for_mock(server: &MockServer) -> super::Enigma2 {
    let addr = server.address();
    let url_str = format!("enigma2://user:pass@{}:{}", addr.ip(), addr.port());
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    super::Enigma2::from_url(&parsed).unwrap()
  }

  fn default_ctx() -> NotifyContext {
    NotifyContext { title: "Test Title".into(), body: "Test Body".into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_send_success() {
    let server = MockServer::start().await;

    Mock::given(method("GET")).and(path_regex("/web/message")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let e2 = enigma2_for_mock(&server);
    let ctx = default_ctx();
    let result = e2.send(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[tokio::test]
  async fn test_send_with_auth() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
      .and(path_regex("/web/message"))
      .and(wiremock::matchers::header_exists("Authorization"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let e2 = enigma2_with_auth_for_mock(&server);
    let ctx = default_ctx();
    let result = e2.send(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[tokio::test]
  async fn test_send_server_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET")).and(path_regex("/web/message")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;

    let e2 = enigma2_for_mock(&server);
    let ctx = default_ctx();
    let result = e2.send(&ctx).await;
    assert!(result.is_ok());
    // Returns false on non-success status
    assert_eq!(result.unwrap(), false);
  }

  #[tokio::test]
  async fn test_send_bizarre_status_code() {
    let server = MockServer::start().await;

    Mock::given(method("GET")).and(path_regex("/web/message")).respond_with(ResponseTemplate::new(418)).expect(1).mount(&server).await;

    let e2 = enigma2_for_mock(&server);
    let ctx = default_ctx();
    let result = e2.send(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), false);
  }

  #[tokio::test]
  async fn test_send_no_title() {
    let server = MockServer::start().await;

    Mock::given(method("GET")).and(path_regex("/web/message")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let e2 = enigma2_for_mock(&server);
    let ctx = NotifyContext { title: "".into(), body: "Just a body".into(), ..Default::default() };
    let result = e2.send(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), true);
  }

  #[test]
  fn test_default_ports() {
    let parsed = crate::utils::parse::ParsedUrl::parse("enigma2://localhost").unwrap();
    let e2 = super::Enigma2::from_url(&parsed).unwrap();
    assert_eq!(e2.port, 80);

    let parsed = crate::utils::parse::ParsedUrl::parse("enigma2s://localhost").unwrap();
    let e2 = super::Enigma2::from_url(&parsed).unwrap();
    assert_eq!(e2.port, 443);
  }

  #[test]
  fn test_custom_port() {
    let parsed = crate::utils::parse::ParsedUrl::parse("enigma2://localhost:8080").unwrap();
    let e2 = super::Enigma2::from_url(&parsed).unwrap();
    assert_eq!(e2.port, 8080);
  }
}
