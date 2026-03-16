use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
pub struct Prowl {
  apikey: String,
  priority: i32,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Prowl {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url.host.clone()?;
    if apikey.is_empty() {
      return None;
    }
    // API key must be 40 characters
    if apikey.len() != 40 {
      return None;
    }
    // Additional API keys in path must also be 40 chars
    for key in &url.path_parts {
      if !key.is_empty() && key.len() != 40 {
        return None;
      }
    }
    let priority = url.get("priority").and_then(|p| p.parse().ok()).unwrap_or(0);
    Some(Self { apikey, priority, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Prowl",
      service_url: Some("https://www.prowlapp.com"),
      setup_url: None,
      protocols: vec!["prowl"],
      description: "Send iOS push notifications via Prowl.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Prowl {
  fn schemas(&self) -> &[&str] {
    &["prowl"]
  }
  fn service_name(&self) -> &str {
    "Prowl"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let params = [
      ("apikey", self.apikey.as_str()),
      ("application", "Apprise"),
      ("event", ctx.title.as_str()),
      ("description", ctx.body.as_str()),
      ("priority", &self.priority.to_string()),
    ];
    let client = build_client(self.verify_certificate)?;
    let resp = client.post("https://api.prowlapp.com/publicapi/add").header("User-Agent", APP_ID).form(&params).send().await?;
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
  fn test_valid_urls() {
    let a40 = "a".repeat(40);
    let b40 = "b".repeat(40);
    let w40 = "w".repeat(40);
    let urls = vec![
      // API key + provider key
      format!("prowl://{}/{}", a40, b40),
      // API key only
      format!("prowl://{}", a40),
      // API key + priority
      format!("prowl://{}?priority=high", a40),
      // API key + invalid priority (defaults)
      format!("prowl://{}?priority=invalid", a40),
      // API key + empty priority
      format!("prowl://{}?priority=", a40),
      // API key with trailing slashes (empty provider key parts filtered)
      format!("prowl://{}///", w40),
      // API key + provider key
      format!("prowl://{}/{}", a40, b40),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let short_key = format!("prowl://{}", "a".repeat(20));
    let bad_provider = format!("prowl://{}/{}", "a".repeat(40), "b".repeat(20));
    let urls: Vec<&str> = vec!["prowl://", "prowl://:@/", &short_key, &bad_provider];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_basic_fields() {
    let a40 = "a".repeat(40);
    let url = format!("prowl://{}", a40);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
    let obj = Prowl::from_url(&parsed).unwrap();
    assert_eq!(obj.apikey, a40);
    assert_eq!(obj.priority, 0);
  }

  #[test]
  fn test_from_url_with_priority() {
    let a40 = "a".repeat(40);
    let url = format!("prowl://{}?priority=2", a40);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
    let obj = Prowl::from_url(&parsed).unwrap();
    assert_eq!(obj.priority, 2);
  }

  #[test]
  fn test_from_url_with_provider_key() {
    let a40 = "a".repeat(40);
    let b40 = "b".repeat(40);
    let url = format!("prowl://{}/{}", a40, b40);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
    let obj = Prowl::from_url(&parsed).unwrap();
    assert_eq!(obj.apikey, a40);
  }

  #[test]
  fn test_service_details() {
    let details = Prowl::static_details();
    assert_eq!(details.service_name, "Prowl");
    assert!(details.protocols.contains(&"prowl"));
    assert_eq!(details.service_url, Some("https://www.prowlapp.com"));
  }
}
