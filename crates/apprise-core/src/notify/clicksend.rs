use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct ClickSend {
  user: String,
  apikey: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl ClickSend {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let user = url.user.clone()?;
    let apikey = url.get("key").map(|s| s.to_string()).or_else(|| url.password.clone())?;
    let mut targets = Vec::new();
    if let Some(h) = url.host.as_deref() {
      if !h.is_empty() && h != "_" {
        targets.push(h.to_string());
      }
    }
    targets.extend(url.path_parts.clone());
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    if targets.is_empty() {
      return None;
    }
    Some(Self { user, apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "ClickSend",
      service_url: Some("https://clicksend.com"),
      setup_url: None,
      protocols: vec!["clicksend"],
      description: "Send SMS via ClickSend.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for ClickSend {
  fn schemas(&self) -> &[&str] {
    &["clicksend"]
  }
  fn service_name(&self) -> &str {
    "ClickSend"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
    let msgs: Vec<_> = self.targets.iter().map(|t| json!({ "to": t, "body": msg, "source": "Apprise" })).collect();
    let payload = json!({ "messages": msgs });
    let client = build_client(self.verify_certificate)?;
    let resp = client
      .post("https://rest.clicksend.com/v3/sms/send")
      .header("User-Agent", APP_ID)
      .basic_auth(&self.user, Some(&self.apikey))
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

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "clicksend://user:pass@33333333333333?batch=yes",
      "clicksend://user:pass@33333333333333?batch=yes&to=66666666666666",
      "clicksend://user:pass@33333333333333?batch=no",
      "clicksend://user@33333333333333?batch=no&key=abc123",
      "clicksend://user:pass@33333333333333",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["clicksend://", "clicksend://:@/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_struct_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("clicksend://myuser:mypass@33333333333333/44444444444444").unwrap();
    let obj = ClickSend::from_url(&parsed).unwrap();
    assert_eq!(obj.user, "myuser");
    assert_eq!(obj.apikey, "mypass");
    assert_eq!(obj.targets.len(), 2);
    assert!(obj.targets.contains(&"33333333333333".to_string()));
    assert!(obj.targets.contains(&"44444444444444".to_string()));
  }

  #[test]
  fn test_from_url_to_query_param() {
    let parsed = crate::utils::parse::ParsedUrl::parse("clicksend://user:pass@33333333333333?to=66666666666666").unwrap();
    let obj = ClickSend::from_url(&parsed).unwrap();
    assert_eq!(obj.targets.len(), 2);
    assert!(obj.targets.contains(&"33333333333333".to_string()));
    assert!(obj.targets.contains(&"66666666666666".to_string()));
  }

  #[test]
  fn test_from_url_key_query_param() {
    let parsed = crate::utils::parse::ParsedUrl::parse("clicksend://user@33333333333333?key=myapikey").unwrap();
    let obj = ClickSend::from_url(&parsed).unwrap();
    assert_eq!(obj.user, "user");
    assert_eq!(obj.apikey, "myapikey");
  }

  #[test]
  fn test_no_targets_returns_none() {
    let parsed = crate::utils::parse::ParsedUrl::parse("clicksend://user:pass@").unwrap();
    assert!(ClickSend::from_url(&parsed).is_none());
  }

  #[test]
  fn test_service_details() {
    let details = ClickSend::static_details();
    assert_eq!(details.service_name, "ClickSend");
    assert_eq!(details.protocols, vec!["clicksend"]);
    assert!(!details.attachment_support);
  }
}
