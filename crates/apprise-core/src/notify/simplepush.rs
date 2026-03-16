use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
pub struct SimplePush {
  apikey: String,
  event: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl SimplePush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url.host.clone()?;
    let event = url.get("event").map(|s| s.to_string());
    Some(Self { apikey, event, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SimplePush",
      service_url: Some("https://simplepush.io"),
      setup_url: None,
      protocols: vec!["spush"],
      description: "Send push notifications via SimplePush.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for SimplePush {
  fn schemas(&self) -> &[&str] {
    &["spush"]
  }
  fn service_name(&self) -> &str {
    "SimplePush"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let mut params = vec![("key", self.apikey.as_str()), ("title", ctx.title.as_str()), ("msg", ctx.body.as_str())];
    let event = self.event.as_deref().unwrap_or("default");
    params.push(("event", event));
    let client = build_client(self.verify_certificate)?;
    let resp = client.post("https://api.simplepush.io/send").header("User-Agent", APP_ID).form(&params).send().await?;
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
      format!("spush://{}", "A".repeat(14)),
      format!("spush://{}", "Y".repeat(14)),
      format!("spush://{}?event=Not%20So%20Good", "X".repeat(14)),
      format!("spush://salt:pass@{}", "X".repeat(14)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["spush://"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let apikey = "Y".repeat(14);
    let url_str = format!("spush://{}", apikey);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    let sp = SimplePush::from_url(&parsed).unwrap();
    assert_eq!(sp.apikey, apikey);
    assert!(sp.event.is_none());
  }

  #[test]
  fn test_from_url_with_event() {
    let apikey = "X".repeat(14);
    let url_str = format!("spush://{}?event=MyEvent", apikey);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    let sp = SimplePush::from_url(&parsed).unwrap();
    assert_eq!(sp.apikey, apikey);
    assert_eq!(sp.event.as_deref(), Some("MyEvent"));
  }

  #[test]
  fn test_from_url_with_salt_and_password() {
    let apikey = "X".repeat(14);
    let url_str = format!("spush://salt:pass@{}", apikey);
    let parsed = crate::utils::parse::ParsedUrl::parse(&url_str).unwrap();
    let sp = SimplePush::from_url(&parsed).unwrap();
    assert_eq!(sp.apikey, apikey);
  }

  #[test]
  fn test_service_details() {
    let d = SimplePush::static_details();
    assert_eq!(d.service_name, "SimplePush");
    assert!(d.protocols.contains(&"spush"));
    assert!(!d.attachment_support);
  }
}
