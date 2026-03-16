use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct WebexTeams {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl WebexTeams {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = url.host.clone().filter(|h| !h.is_empty() && h != "_").or_else(|| url.get("token").map(|s| s.to_string()))?;
    if token.is_empty() {
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Cisco Webex Teams",
      service_url: Some("https://webex.com"),
      setup_url: None,
      protocols: vec!["wxteams", "webex"],
      description: "Send via Cisco Webex Teams incoming webhooks.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for WebexTeams {
  fn schemas(&self) -> &[&str] {
    &["wxteams", "webex"]
  }
  fn service_name(&self) -> &str {
    "Cisco Webex Teams"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // Python sends body only (no title), and chooses text vs markdown based on format
    let payload = if ctx.body_format == crate::types::NotifyFormat::Text { json!({ "text": ctx.body }) } else { json!({ "markdown": ctx.body }) };
    let client = build_client(self.verify_certificate)?;
    let url = format!("https://api.ciscospark.com/v1/webhooks/incoming/{}", self.token);
    let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
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
  use crate::notify::registry::from_url;

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["wxteams://", "wxteams://:@/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls: Vec<String> = vec![
      // Token via host
      format!("wxteams://{}", "a".repeat(80)),
      // Token via query param
      format!("wxteams://?token={}", "a".repeat(80)),
      // webex schema
      format!("webex://{}", "a".repeat(140)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_token_from_host() {
    let token = "a".repeat(80);
    let parsed = ParsedUrl::parse(&format!("wxteams://{}", token)).unwrap();
    let wt = WebexTeams::from_url(&parsed).unwrap();
    assert_eq!(wt.token, token);
  }

  #[test]
  fn test_from_url_token_from_query() {
    let token = "b".repeat(80);
    let parsed = ParsedUrl::parse(&format!("wxteams://?token={}", token)).unwrap();
    let wt = WebexTeams::from_url(&parsed).unwrap();
    assert_eq!(wt.token, token);
  }

  #[test]
  fn test_from_url_webex_schema() {
    let token = "c".repeat(140);
    let parsed = ParsedUrl::parse(&format!("webex://{}", token)).unwrap();
    let wt = WebexTeams::from_url(&parsed).unwrap();
    assert_eq!(wt.token, token);
  }

  #[test]
  fn test_service_details() {
    let details = WebexTeams::static_details();
    assert_eq!(details.service_name, "Cisco Webex Teams");
    assert_eq!(details.service_url, Some("https://webex.com"));
    assert!(details.protocols.contains(&"wxteams"));
    assert!(details.protocols.contains(&"webex"));
    assert!(!details.attachment_support);
  }
}
