use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct SpugPush {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl SpugPush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = url
      .get("token")
      .map(|s| s.to_string())
      .or_else(|| if url.schema == "https" || url.schema == "http" { url.path_parts.last().cloned() } else { url.host.clone().filter(|h| !h.is_empty()) })?;
    if token.contains('!') || token.contains('%') || token.trim().is_empty() {
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SpugPush",
      service_url: Some("https://spug.cc"),
      setup_url: None,
      protocols: vec!["spugpush"],
      description: "Send push via Spug.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for SpugPush {
  fn schemas(&self) -> &[&str] {
    &["spugpush"]
  }
  fn service_name(&self) -> &str {
    "SpugPush"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({ "title": ctx.title, "content": ctx.body });
    let url = format!("https://push.spug.cc/send/{}", self.token);
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
      "spugpush://abc123def456ghi789jkl012mno345pq",
      "spugpush://?token=abc123def456ghi789jkl012mno345pq",
      "https://push.spug.dev/send/abc123def456ghi789jkl012mno345pq",
      "spugpush://ffffffffffffffffffffffffffffffff",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["spugpush://", "spugpush://invalid!"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_host_form() {
    let parsed = crate::utils::parse::ParsedUrl::parse("spugpush://abc123def456ghi789jkl012mno345pq").unwrap();
    let sp = SpugPush::from_url(&parsed).unwrap();
    assert_eq!(sp.token, "abc123def456ghi789jkl012mno345pq");
  }

  #[test]
  fn test_from_url_token_param() {
    let parsed = crate::utils::parse::ParsedUrl::parse("spugpush://?token=abc123def456ghi789jkl012mno345pq").unwrap();
    let sp = SpugPush::from_url(&parsed).unwrap();
    assert_eq!(sp.token, "abc123def456ghi789jkl012mno345pq");
  }

  #[test]
  fn test_from_url_https_form() {
    let parsed = crate::utils::parse::ParsedUrl::parse("https://push.spug.dev/send/abc123def456ghi789jkl012mno345pq").unwrap();
    let sp = SpugPush::from_url(&parsed).unwrap();
    assert_eq!(sp.token, "abc123def456ghi789jkl012mno345pq");
  }

  #[test]
  fn test_rejects_exclamation_mark() {
    let parsed = crate::utils::parse::ParsedUrl::parse("spugpush://invalid!").unwrap();
    assert!(SpugPush::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = SpugPush::static_details();
    assert_eq!(details.service_name, "SpugPush");
    assert_eq!(details.service_url, Some("https://spug.cc"));
    assert!(details.protocols.contains(&"spugpush"));
    assert!(!details.attachment_support);
  }
}
