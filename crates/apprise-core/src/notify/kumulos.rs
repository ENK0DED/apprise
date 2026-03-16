use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
#[allow(dead_code)]
pub struct Kumulos {
  apikey: String,
  server_key: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Kumulos {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // kumulos://UUID/server_key  (UUID is host, server_key is first path part)
    // or kumulos://user:pass@host format
    let (apikey, server_key) = if url.password.is_some() {
      (url.user.clone()?, url.password.clone()?)
    } else {
      let apikey = url.host.clone()?;
      let server_key = url.path_parts.first()?.clone();
      if server_key.is_empty() {
        return None;
      }
      (apikey, server_key)
    };
    if apikey.is_empty() || server_key.is_empty() {
      return None;
    }
    // Server key should be at least 36 chars
    if server_key.len() < 36 {
      return None;
    }
    let targets = if url.password.is_some() { url.path_parts.clone() } else { url.path_parts.get(1..).unwrap_or(&[]).to_vec() };
    Some(Self { apikey, server_key, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Kumulos",
      service_url: Some("https://kumulos.com"),
      setup_url: None,
      protocols: vec!["kumulos"],
      description: "Send push notifications via Kumulos.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Kumulos {
  fn schemas(&self) -> &[&str] {
    &["kumulos"]
  }
  fn service_name(&self) -> &str {
    "Kumulos"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = json!({ "target": { "broadcast": true }, "content": { "title": ctx.title, "message": ctx.body } });
    let client = build_client(self.verify_certificate)?;
    let resp = client
      .post("https://messages.kumulos.com/v2/notifications")
      .header("User-Agent", APP_ID)
      .basic_auth(&self.apikey, Some(&self.server_key))
      .json(&payload)
      .send()
      .await?;
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

  const UUID4: &str = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";

  fn parse_kumulos(url: &str) -> Option<Kumulos> {
    ParsedUrl::parse(url).and_then(|p| Kumulos::from_url(&p))
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "kumulos://".to_string(),
      "kumulos://:@/".to_string(),
      // No server key
      format!("kumulos://{}/", UUID4),
    ];
    for url in &urls {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let server_key = "w".repeat(36);
    let urls = vec![format!("kumulos://{}/{}/", UUID4, server_key)];
    for url in &urls {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let server_key = "w".repeat(36);
    let obj = parse_kumulos(&format!("kumulos://{}/{}/", UUID4, server_key)).unwrap();
    assert_eq!(obj.apikey, UUID4);
    assert_eq!(obj.server_key, server_key);
  }

  #[test]
  fn test_server_key_too_short() {
    // Server key must be at least 36 chars
    let obj = parse_kumulos(&format!("kumulos://{}/short/", UUID4));
    assert!(obj.is_none());
  }

  #[test]
  fn test_service_details() {
    let details = Kumulos::static_details();
    assert_eq!(details.service_name, "Kumulos");
    assert_eq!(details.protocols, vec!["kumulos"]);
  }
}
