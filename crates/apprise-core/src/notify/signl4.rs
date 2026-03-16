use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct Signl4 {
  secret: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Signl4 {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let secret = url.host.clone().filter(|h| !h.is_empty()).or_else(|| url.get("secret").map(|s| s.to_string()))?;
    let decoded = urlencoding::decode(&secret).unwrap_or_default().into_owned();
    if decoded.trim().is_empty() {
      return None;
    }
    Some(Self { secret: decoded, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SIGNL4",
      service_url: Some("https://www.signl4.com"),
      setup_url: None,
      protocols: vec!["signl4"],
      description: "Send mobile alerts via SIGNL4.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Signl4 {
  fn schemas(&self) -> &[&str] {
    &["signl4"]
  }
  fn service_name(&self) -> &str {
    "SIGNL4"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("https://connect.signl4.com/webhook/{}/", self.secret);
    let payload = json!({ "Title": ctx.title, "Description": ctx.body });
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
      "signl4://secret/",
      "signl4://?secret=secret",
      "signl4://secret/?service=IoT",
      "signl4://secret/?filtering=yes",
      "signl4://secret/?location=40.6413111,-73.7781391",
      "signl4://secret/?alerting_scenario=singl4_ack",
      "signl4://secret/?filtering=False",
      "signl4://secret/?external_id=ar1234&status=new",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["signl4://", "signl4://:@/", "signl4://%20%20/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_host_secret() {
    let parsed = crate::utils::parse::ParsedUrl::parse("signl4://mysecret/").unwrap();
    let s = Signl4::from_url(&parsed).unwrap();
    assert_eq!(s.secret, "mysecret");
  }

  #[test]
  fn test_from_url_query_secret() {
    let parsed = crate::utils::parse::ParsedUrl::parse("signl4://?secret=mysecret").unwrap();
    let s = Signl4::from_url(&parsed).unwrap();
    assert_eq!(s.secret, "mysecret");
  }

  #[test]
  fn test_whitespace_only_secret_rejected() {
    let parsed = crate::utils::parse::ParsedUrl::parse("signl4://%20%20/").unwrap();
    assert!(Signl4::from_url(&parsed).is_none());
  }

  #[test]
  fn test_service_details() {
    let d = Signl4::static_details();
    assert_eq!(d.service_name, "SIGNL4");
    assert!(d.protocols.contains(&"signl4"));
    assert!(!d.attachment_support);
  }
}
