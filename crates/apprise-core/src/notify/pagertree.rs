use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct PagerTree {
  integration_id: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl PagerTree {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let integration_id =
      url.get("id").or_else(|| url.get("integration")).map(|s| s.to_string()).or_else(|| url.host.clone().filter(|h| !h.is_empty() && h != "_"))?;
    if integration_id.is_empty() {
      return None;
    }
    // Reject if all non-alphanumeric (e.g., all plus signs decoded to spaces)
    let decoded = urlencoding::decode(&integration_id).unwrap_or_default();
    if decoded.trim().is_empty() {
      return None;
    }
    if !decoded.chars().any(|c| c.is_ascii_alphanumeric()) {
      return None;
    }
    Some(Self { integration_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "PagerTree",
      service_url: Some("https://pagertree.com"),
      setup_url: None,
      protocols: vec!["pagertree"],
      description: "Send alerts via PagerTree.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for PagerTree {
  fn schemas(&self) -> &[&str] {
    &["pagertree"]
  }
  fn service_name(&self) -> &str {
    "PagerTree"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = json!({ "event_type": "create", "title": ctx.title, "description": ctx.body });
    let url = format!("https://api.pagertree.com/integration/{}", self.integration_id);
    let client = build_client(self.verify_certificate)?;
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
  fn test_valid_urls() {
    let urls = vec![
      "pagertree://int_xxxxxxxxxxx",
      "pagertree://int_xxxxxxxxxxx?integration=int_yyyyyyyyyy",
      "pagertree://int_xxxxxxxxxxx?id=int_zzzzzzzzzz",
      "pagertree://int_xxxxxxxxxxx?urgency=low",
      "pagertree://?id=int_xxxxxxxxxxx&urgency=low",
      "pagertree://int_xxxxxxxxxxx?tags=production,web",
      "pagertree://int_xxxxxxxxxxx?action=resolve&thirdparty=123",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let plus_url = format!("pagertree://{}", "+".repeat(24));
    let urls: Vec<&str> = vec![
      "pagertree://",
      "pagertree://:@/",
      // All plus signs (decoded to spaces)
      &plus_url,
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_basic_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("pagertree://int_xxxxxxxxxxx").unwrap();
    let obj = PagerTree::from_url(&parsed).unwrap();
    assert_eq!(obj.integration_id, "int_xxxxxxxxxxx");
  }

  #[test]
  fn test_from_url_id_override() {
    // id= query param overrides host
    let parsed = crate::utils::parse::ParsedUrl::parse("pagertree://int_xxxxxxxxxxx?id=int_zzzzzzzzzz").unwrap();
    let obj = PagerTree::from_url(&parsed).unwrap();
    assert_eq!(obj.integration_id, "int_zzzzzzzzzz");
  }

  #[test]
  fn test_from_url_integration_override() {
    let parsed = crate::utils::parse::ParsedUrl::parse("pagertree://int_xxxxxxxxxxx?integration=int_yyyyyyyyyy").unwrap();
    let obj = PagerTree::from_url(&parsed).unwrap();
    assert_eq!(obj.integration_id, "int_yyyyyyyyyy");
  }

  #[test]
  fn test_service_details() {
    let details = PagerTree::static_details();
    assert_eq!(details.service_name, "PagerTree");
    assert!(details.protocols.contains(&"pagertree"));
    assert_eq!(details.service_url, Some("https://pagertree.com"));
  }
}
