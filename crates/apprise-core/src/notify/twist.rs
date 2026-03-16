use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct Twist {
  token: String,
  channel_id: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Twist {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // twist://email:password or twist://email/password
    let token = url.password.clone().or_else(|| url.path_parts.first().cloned())?;
    let channel_id = url.host.clone().unwrap_or_default();
    Some(Self { token, channel_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Twist",
      service_url: Some("https://twist.com"),
      setup_url: None,
      protocols: vec!["twist"],
      description: "Send messages via Twist.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Twist {
  fn schemas(&self) -> &[&str] {
    &["twist"]
  }
  fn service_name(&self) -> &str {
    "Twist"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({ "channel_id": self.channel_id, "title": ctx.title, "content": ctx.body });
    let resp = client.post("https://api.twist.com/api/v3/threads/add").header("User-Agent", APP_ID).bearer_auth(&self.token).json(&payload).send().await?;
    Ok(resp.status().is_success())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec!["twist://user@example.com/password", "twist://password:user1@example.com", "twist://password:user2@example.com"];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["twist://", "twist://:@/", "twist://user@example.com/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_password_in_path() {
    // twist://user@example.com/password
    let parsed = ParsedUrl::parse("twist://user@example.com/password").unwrap();
    let tw = Twist::from_url(&parsed).unwrap();
    assert_eq!(tw.token, "password");
  }

  #[test]
  fn test_from_url_password_colon_form() {
    // twist://password:user@example.com
    // URL parsing: user=password, password=user, host=example.com
    let parsed = ParsedUrl::parse("twist://password:user@example.com").unwrap();
    let tw = Twist::from_url(&parsed).unwrap();
    // password field from URL is "user"
    assert_eq!(tw.token, "user");
  }

  #[test]
  fn test_from_url_channel_in_path() {
    let parsed = ParsedUrl::parse("twist://password:user@example.com/12345").unwrap();
    let tw = Twist::from_url(&parsed).unwrap();
    assert!(tw.token.len() > 0);
  }

  #[test]
  fn test_service_details() {
    let details = Twist::static_details();
    assert_eq!(details.service_name, "Twist");
    assert_eq!(details.service_url, Some("https://twist.com"));
    assert!(details.protocols.contains(&"twist"));
    assert!(!details.attachment_support);
  }
}
