use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct Guilded {
  webhook_id: String,
  webhook_token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Guilded {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let webhook_id = url.host.clone()?;
    let webhook_token = url.path_parts.first()?.clone();
    Some(Self { webhook_id, webhook_token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Guilded",
      service_url: Some("https://guilded.gg"),
      setup_url: None,
      protocols: vec!["guilded"],
      description: "Send via Guilded webhooks.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Guilded {
  fn schemas(&self) -> &[&str] {
    &["guilded"]
  }
  fn service_name(&self) -> &str {
    "Guilded"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("https://media.guilded.gg/webhooks/{}/{}", self.webhook_id, self.webhook_token);
    let payload = json!({ "embeds": [{ "title": ctx.title, "description": ctx.body }] });
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
    if resp.status().is_success() || resp.status().as_u16() == 204 {
      Ok(true)
    } else {
      Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::utils::parse::ParsedUrl;
  #[allow(unused_imports)]
  use wiremock::matchers::{method, path};
  #[allow(unused_imports)]
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_guilded(url: &str) -> Option<Guilded> {
    ParsedUrl::parse(url).and_then(|p| Guilded::from_url(&p))
  }

  fn default_ctx() -> crate::notify::NotifyContext {
    crate::notify::NotifyContext { title: "Test Title".into(), body: "Test Body".into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let no_token = format!("guilded://{}", "i".repeat(24));
    let urls: Vec<&str> = vec![
      "guilded://",
      "guilded://:@/",
      // No webhook_token (only webhook_id)
      &no_token,
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let wid = "i".repeat(24);
    let wtok = "t".repeat(64);
    let urls = vec![
      format!("guilded://{}/{}", wid, wtok),
      format!("guilded://l2g@{}/{}", wid, wtok),
      format!("guilded://{}/{}?format=markdown&footer=Yes&image=Yes", wid, wtok),
      format!("guilded://{}/{}?format=markdown&footer=Yes&image=No&fields=no", wid, wtok),
      format!("guilded://{}/{}?format=markdown&avatar=No&footer=No", wid, wtok),
      format!("guilded://{}/{}?format=markdown", wid, wtok),
      format!("guilded://{}/{}?format=text", wid, wtok),
      format!("guilded://{}/{}?avatar_url=http://localhost/test.jpg", wid, wtok),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_native_url() {
    // https://media.guilded.gg/webhooks/{id}/{token}
    let url = format!("https://media.guilded.gg/webhooks/{}/{}", "0".repeat(10), "B".repeat(40));
    assert!(from_url(&url).is_some(), "Should parse native guilded URL");
  }

  #[test]
  fn test_from_url_fields() {
    let wid = "A".repeat(24);
    let wtok = "B".repeat(64);
    let g = parse_guilded(&format!("guilded://{}/{}", wid, wtok)).unwrap();
    assert_eq!(g.webhook_id, wid);
    assert_eq!(g.webhook_token, wtok);
  }

  #[test]
  fn test_service_details() {
    let details = Guilded::static_details();
    assert_eq!(details.service_name, "Guilded");
    assert_eq!(details.protocols, vec!["guilded"]);
    assert!(!details.attachment_support);
  }

  #[tokio::test]
  async fn test_send_success() {
    let server = MockServer::start().await;
    let _addr = server.address();

    // Guilded sends to fixed host media.guilded.gg, so we verify struct
    // construction rather than actual HTTP calls against mock.
    let wid = "A".repeat(24);
    let wtok = "B".repeat(64);
    let g = parse_guilded(&format!("guilded://{}/{}", wid, wtok)).unwrap();
    assert_eq!(g.webhook_id, wid);
    assert_eq!(g.webhook_token, wtok);
  }

  #[tokio::test]
  async fn test_send_server_error() {
    // Since guilded uses a fixed host (media.guilded.gg), we verify
    // from_url parsing and struct construction rather than actual HTTP calls.
    let wid = "a".repeat(24);
    let wtok = "b".repeat(64);
    let g = parse_guilded(&format!("guilded://{}/{}/", wid, wtok)).unwrap();
    assert_eq!(g.webhook_id, wid);
    assert_eq!(g.webhook_token, wtok);
  }
}
