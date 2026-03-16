use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct TechulusPush {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl TechulusPush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = url.host.clone()?;
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "TechulusPush",
      service_url: Some("https://push.techulus.com"),
      setup_url: None,
      protocols: vec!["push", "techuluspush"],
      description: "Send push via Techulus.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for TechulusPush {
  fn schemas(&self) -> &[&str] {
    &["techulus", "push", "techuluspush"]
  }
  fn service_name(&self) -> &str {
    "TechulusPush"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({ "title": ctx.title, "body": ctx.body });
    let resp =
      client.post("https://push.techulus.com/api/v1/notify").header("User-Agent", APP_ID).header("x-api-key", &self.token).json(&payload).send().await?;
    Ok(resp.status().is_success())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  #[test]
  fn test_invalid_urls() {
    // techulus:// is the registered schema for TechulusPush in the registry
    let urls = vec!["techulus://"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let uuid = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";
    let urls = vec![format!("techulus://{}", uuid)];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let uuid = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("techulus://{}", uuid)).unwrap();
    let tp = TechulusPush::from_url(&parsed).unwrap();
    assert_eq!(tp.token, uuid);
  }

  #[test]
  fn test_from_url_empty_host_returns_none() {
    let parsed = crate::utils::parse::ParsedUrl::parse("techulus://").unwrap();
    // host is None or empty => from_url returns None
    assert!(TechulusPush::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = TechulusPush::static_details();
    assert_eq!(details.service_name, "TechulusPush");
    assert_eq!(details.service_url, Some("https://push.techulus.com"));
    assert!(details.protocols.contains(&"push"));
    assert!(details.protocols.contains(&"techuluspush"));
    assert!(!details.attachment_support);
  }

  #[test]
  fn test_schemas() {
    let uuid = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";
    let parsed = crate::utils::parse::ParsedUrl::parse(&format!("techulus://{}", uuid)).unwrap();
    let tp = TechulusPush::from_url(&parsed).unwrap();
    let schemas = tp.schemas();
    assert!(schemas.contains(&"push"));
    assert!(schemas.contains(&"techuluspush"));
  }
}
