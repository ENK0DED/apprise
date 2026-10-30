use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

/// Feishu bot webhook: `feishu://{token}`.
pub struct FeiShu {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl FeiShu {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => t.to_string(),
      None => url.host.clone().unwrap_or_default(),
    };
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
      tracing::warn!("The Feishu token specified is invalid.");
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Feishu",
      service_url: Some("https://open.feishu.cn/"),
      setup_url: Some("https://appriseit.com/services/feishu/"),
      protocols: vec!["feishu"],
      description: "Send via Feishu bot webhook.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for FeiShu {
  fn schemas(&self) -> &[&str] {
    &["feishu"]
  }
  fn service_name(&self) -> &str {
    "Feishu"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    19985
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("{}/open-apis/bot/v2/hook/{}/", service_base("https://open.feishu.cn"), self.token);
    let payload = json!({ "msg_type": "text", "content": { "text": ctx.body } });
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
      tracing::info!("Sent Feishu notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send Feishu notification: error={}", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec!["feishu://abc123", "feishu://?token=abc123", "feishu://token", "feishu://abc-123_X", "feishu://a%2Db"];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["feishu://", "feishu://:@/", "feishu://%badtoken%", "feishu://abc.def", "feishu://?token=a!b"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let f = FeiShu::from_url(&ParsedUrl::parse("feishu://abc123").unwrap()).unwrap();
    assert_eq!(f.token, "abc123");
    // ?token= takes priority over the host
    let f = FeiShu::from_url(&ParsedUrl::parse("feishu://host?token=abc123").unwrap()).unwrap();
    assert_eq!(f.token, "abc123");
  }

  #[test]
  fn test_service_details() {
    let details = FeiShu::static_details();
    assert_eq!(details.service_name, "Feishu");
    assert_eq!(details.protocols, vec!["feishu"]);
    assert!(!details.attachment_support);
    let svc = from_url("feishu://abc123").unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 19985);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/open-apis/bot/v2/hook/abc-123_X/"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({ "msg_type": "text", "content": { "text": "T\r\nB" } })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("feishu://abc-123_X").unwrap();
    let ctx = NotifyContext { body: "T\r\nB".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/open-apis/bot/v2/hook/token/")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url("feishu://token").unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err());
    }
  }
}
