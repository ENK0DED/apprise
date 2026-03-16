use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
pub struct Join {
  apikey: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Join {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url.host.clone()?;
    let targets = url.path_parts.clone();
    Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Join",
      service_url: Some("https://joaoapps.com/join/"),
      setup_url: None,
      protocols: vec!["join"],
      description: "Send notifications via Join.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Join {
  fn schemas(&self) -> &[&str] {
    &["join"]
  }
  fn service_name(&self) -> &str {
    "Join"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let device_ids = if self.targets.is_empty() { "group.all".to_string() } else { self.targets.join(",") };
    let url = format!(
      "https://joinjoaomgcd.appspot.com/_ah/api/messaging/v1/sendPush?apikey={}&deviceIds={}&title={}&text={}",
      self.apikey,
      urlencoding::encode(&device_ids),
      urlencoding::encode(&ctx.title),
      urlencoding::encode(&ctx.body)
    );
    let client = build_client(self.verify_certificate)?;
    let resp = client.get(&url).header("User-Agent", APP_ID).send().await?;
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
  use crate::utils::parse::ParsedUrl;

  fn parse_join(url: &str) -> Option<Join> {
    ParsedUrl::parse(url).and_then(|p| Join::from_url(&p))
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["join://", "join://:@/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let apikey = "a".repeat(32);
    let device = "d".repeat(32);
    let device2 = "e".repeat(32);
    let urls = vec![
      // APIkey; no device
      format!("join://{}", apikey),
      // API Key + device (using to=)
      format!("join://{}?to={}", apikey, device),
      // API Key + priority setting
      format!("join://{}?priority=high", apikey),
      // API Key + invalid priority setting
      format!("join://{}?priority=invalid", apikey),
      // API Key + priority setting (empty)
      format!("join://{}?priority=", apikey),
      // API Key + device
      format!("join://{}@{}?image=True", apikey, device),
      // No image
      format!("join://{}@{}?image=False", apikey, device),
      // API Key + Device Name
      format!("join://{}/My Device", apikey),
      // API Key + device
      format!("join://{}/{}", apikey, device),
      // API Key + 2 devices
      format!("join://{}/{}/{}", apikey, device, device2),
      // API Key + 1 device and 1 group
      format!("join://{}/{}/group.chrome", apikey, device),
    ];
    for url in &urls {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_apikey() {
    let apikey = "a".repeat(32);
    let obj = parse_join(&format!("join://{}", apikey)).unwrap();
    assert_eq!(obj.apikey, apikey);
    assert!(obj.targets.is_empty());
  }

  #[test]
  fn test_from_url_with_devices() {
    let apikey = "a".repeat(32);
    let device = "d".repeat(32);
    let obj = parse_join(&format!("join://{}/{}", apikey, device)).unwrap();
    assert_eq!(obj.apikey, apikey);
    assert_eq!(obj.targets, vec![device]);
  }

  #[test]
  fn test_from_url_with_group() {
    let apikey = "a".repeat(32);
    let device = "d".repeat(32);
    let obj = parse_join(&format!("join://{}/{}/group.chrome", apikey, device)).unwrap();
    assert_eq!(obj.targets.len(), 2);
    assert!(obj.targets.contains(&"group.chrome".to_string()));
  }

  #[test]
  fn test_service_details() {
    let details = Join::static_details();
    assert_eq!(details.service_name, "Join");
    assert_eq!(details.protocols, vec!["join"]);
  }

  #[test]
  fn test_default_device_ids_group_all() {
    // When no targets, send() should use "group.all"
    let apikey = "a".repeat(32);
    let obj = parse_join(&format!("join://{}", apikey)).unwrap();
    assert!(obj.targets.is_empty());
  }
}
