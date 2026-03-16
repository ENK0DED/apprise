use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct Spike {
  channel_key: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Spike {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let channel_key = if url.schema == "https" || url.schema == "http" {
      // Extract token from path: /v1/alerts/TOKEN or similar
      url.path_parts.last().cloned()
    } else {
      url.host.clone().filter(|h| !h.is_empty())
    }
    .or_else(|| url.get("token").map(|s| s.to_string()))?;
    // Validate: must be alphanumeric (no hyphens, special chars)
    if channel_key.trim().is_empty() || !channel_key.chars().all(|c| c.is_ascii_alphanumeric()) {
      return None;
    }
    Some(Self { channel_key, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Spike",
      service_url: Some("https://spike.sh"),
      setup_url: None,
      protocols: vec!["spike"],
      description: "Send alerts via Spike.sh.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Spike {
  fn schemas(&self) -> &[&str] {
    &["spike"]
  }
  fn service_name(&self) -> &str {
    "Spike"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({ "title": ctx.title, "message": ctx.body, "status": ctx.notify_type.to_string() });
    let url = format!("https://api.spike.sh/api/v1/integration/webhook/{}", self.channel_key);
    let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
    Ok(resp.status().is_success())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "spike://1234567890abcdef1234567890abcdef",
      "spike://?token=1234567890abcdef1234567890abcdef",
      "https://api.spike.sh/v1/alerts/1234567890abcdef1234567890abcdef",
      "spike://ffffffffffffffffffffffffffffffff",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["spike://", "spike://invalid-key"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_host_form() {
    let parsed = crate::utils::parse::ParsedUrl::parse("spike://1234567890abcdef1234567890abcdef").unwrap();
    let spike = Spike::from_url(&parsed).unwrap();
    assert_eq!(spike.channel_key, "1234567890abcdef1234567890abcdef");
  }

  #[test]
  fn test_from_url_token_param() {
    let parsed = crate::utils::parse::ParsedUrl::parse("spike://?token=1234567890abcdef1234567890abcdef").unwrap();
    let spike = Spike::from_url(&parsed).unwrap();
    assert_eq!(spike.channel_key, "1234567890abcdef1234567890abcdef");
  }

  #[test]
  fn test_from_url_https_form() {
    let parsed = crate::utils::parse::ParsedUrl::parse("https://api.spike.sh/v1/alerts/1234567890abcdef1234567890abcdef").unwrap();
    let spike = Spike::from_url(&parsed).unwrap();
    assert_eq!(spike.channel_key, "1234567890abcdef1234567890abcdef");
  }

  #[test]
  fn test_from_url_rejects_non_alphanumeric() {
    let parsed = crate::utils::parse::ParsedUrl::parse("spike://invalid-key").unwrap();
    assert!(Spike::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = Spike::static_details();
    assert_eq!(details.service_name, "Spike");
    assert_eq!(details.service_url, Some("https://spike.sh"));
    assert!(details.protocols.contains(&"spike"));
    assert!(!details.attachment_support);
  }
}
