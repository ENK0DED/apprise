use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;

pub struct Chanify {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Chanify {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // ?token= wins over the host; upstream unquotes it a second time
    let token = url.get("token").filter(|t| !t.is_empty()).map(unquote).or_else(|| url.host.clone()).unwrap_or_default();
    let token = token.trim().to_string();
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
      tracing::warn!("The Chanify token specified is invalid.");
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Chanify",
      service_url: Some("https://chanify.net/"),
      setup_url: Some("https://appriseit.com/services/chanify/"),
      protocols: vec!["chanify"],
      description: "Send notifications via Chanify.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Chanify {
  fn schemas(&self) -> &[&str] {
    &["chanify"]
  }
  fn service_name(&self) -> &str {
    "Chanify"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // The title is merged into the body
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("{}/v1/sender/{}/", service_base("https://api.chanify.net"), self.token);
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client.post(&url).header("User-Agent", APP_ID).form(&[("text", ctx.body.as_str())]).send().await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent Chanify notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send Chanify notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    for url in ["chanify://abc123", "chanify://?token=abc123", "chanify://token", "chanify://tok.en_-1"] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["chanify://", "chanify://:@/", "chanify://%badtoken%", "chanify://bad!token"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_token_sources() {
    let token = |url: &str| Chanify::from_url(&ParsedUrl::parse(url).unwrap()).unwrap().token;
    assert_eq!(token("chanify://my-secret-token"), "my-secret-token");
    assert_eq!(token("chanify://host?token=abc123"), "abc123");
    // An empty token= falls back to the host
    assert_eq!(token("chanify://host?token="), "host");
  }

  #[tokio::test]
  async fn test_send() {
    // Matches pycapture.py: the (merged) body only, to the token's sender path
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/v1/sender/tok.en-1/"))
      .and(header("content-type", "application/x-www-form-urlencoded"))
      .and(body_string("text=T%0D%0AB"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("chanify://tok.en-1").unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    let ctx = NotifyContext { body: "T\r\nB".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999, 204] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/v1/sender/token/")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), from_url("chanify://token").unwrap().send(&ctx)).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Chanify::static_details();
    assert_eq!(details.service_name, "Chanify");
    assert_eq!(details.protocols, vec!["chanify"]);
  }
}
