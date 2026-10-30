use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

/// ServerChan: `schan://{token}`.
pub struct ServerChan {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl ServerChan {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream matches `schan://([a-zA-Z0-9]+)/` against the raw URL, with
    // the slash optional unless the URL ends in one. The token is the run of
    // alphanumerics right after the schema.
    let rest = url.raw.strip_prefix("schan://")?;
    let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
    let token = &rest[..end];
    if token.is_empty() || (url.raw.ends_with('/') && !rest[end..].starts_with('/')) {
      tracing::warn!("An invalid ServerChan API Token was specified.");
      return None;
    }
    Some(Self { token: token.to_string(), verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "ServerChan",
      service_url: Some("https://sct.ftqq.com/"),
      setup_url: Some("https://appriseit.com/services/serverchan/"),
      protocols: vec!["schan"],
      description: "Send notifications via ServerChan (WeChat).",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for ServerChan {
  fn schemas(&self) -> &[&str] {
    &["schan"]
  }
  fn service_name(&self) -> &str {
    "ServerChan"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("{}/{}.send", service_base("https://sctapi.ftqq.com"), self.token);
    let params = [("title", ctx.title.as_str()), ("desp", ctx.body.as_str())];
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).form(&params).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
      tracing::info!("Sent ServerChan notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send ServerChan notification: error={}", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn token_of(url: &str) -> Option<String> {
    ServerChan::from_url(&ParsedUrl::parse(url).unwrap()).map(|s| s.token)
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec!["schan://12345678".to_string(), format!("schan://{}", "a".repeat(8)), "schan://abc/def/".into(), "schan://abc?x=y".into()];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["schan://", "schan://a_bd_/", "schan://_abc", "schan://?token=abc"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_token_is_leading_alphanumeric_run() {
    assert_eq!(token_of("schan://12345678").as_deref(), Some("12345678"));
    assert_eq!(token_of("schan://12345678/").as_deref(), Some("12345678"));
    // Like upstream, a URL without a trailing slash keeps the leading run
    assert_eq!(token_of("schan://abc_def").as_deref(), Some("abc"));
    assert_eq!(token_of("schan://abc/def/").as_deref(), Some("abc"));
    // The schema must be written in lower case
    assert_eq!(token_of("SCHAN://abc"), None);
  }

  #[test]
  fn test_service_details() {
    let d = ServerChan::static_details();
    assert_eq!(d.service_name, "ServerChan");
    assert!(d.protocols.contains(&"schan"));
    assert!(!d.attachment_support);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/12345678.send"))
      .and(body_string("title=T&desp=B"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("schan://12345678").unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url("schan://12345678").unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.is_err());
    }
  }
}
