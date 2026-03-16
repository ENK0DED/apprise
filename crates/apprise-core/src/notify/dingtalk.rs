use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct DingTalk {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl DingTalk {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Token from ?token= or host
    let token = url.get("token").map(|s| s.to_string()).or_else(|| url.host.clone())?;
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
      return None;
    }
    // Validate secret if provided (must be alphanumeric)
    if let Some(secret) = url.get("secret") {
      if !secret.is_empty() && !secret.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
      }
    }
    // Also check user-field as secret (dingtalk://secret@token/...)
    if let Some(ref user_secret) = url.user {
      if !user_secret.is_empty() && !user_secret.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
      }
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "DingTalk",
      service_url: Some("https://dingtalk.com"),
      setup_url: None,
      protocols: vec!["dingtalk"],
      description: "Send via DingTalk robot webhook.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for DingTalk {
  fn schemas(&self) -> &[&str] {
    &["dingtalk"]
  }
  fn service_name(&self) -> &str {
    "DingTalk"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("https://oapi.dingtalk.com/robot/send?access_token={}", self.token);
    let content = if ctx.title.is_empty() { ctx.body.clone() } else { format!("## {}\n{}", ctx.title, ctx.body) };
    let payload = json!({ "msgtype": "markdown", "markdown": { "title": ctx.title, "text": content } });
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
      "dingtalk://12345678",
      "dingtalk://aaaaaaaa/11111111111111",
      "dingtalk://aaaaaaaa/111/invalid",
      "dingtalk://aaaaaaaa/?to=11111111111111",
      "dingtalk://secret@aaaaaaaa/?to=11111111111111",
      "dingtalk://?token=bbbbbbbb&to=11111111111111&secret=aaaaaaaaaaaaaaa",
      "dingtalk://aaaaaaaa?format=markdown",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "dingtalk://",
      "dingtalk://a_bd_/",
      // Invalid secret (underscore)
      "dingtalk://aaaaaaaa/?to=11111111111111&secret=_",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn parse_dingtalk(url: &str) -> DingTalk {
    let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
    DingTalk::from_url(&parsed).unwrap()
  }

  #[test]
  fn test_from_url_token_from_host() {
    let d = parse_dingtalk("dingtalk://12345678");
    assert_eq!(d.token, "12345678");
  }

  #[test]
  fn test_from_url_token_from_query() {
    let d = parse_dingtalk("dingtalk://?token=bbbbbbbb&to=11111111111111");
    assert_eq!(d.token, "bbbbbbbb");
  }

  #[test]
  fn test_from_url_secret_in_user_field() {
    let d = parse_dingtalk("dingtalk://secret@aaaaaaaa");
    assert_eq!(d.token, "aaaaaaaa");
  }

  #[test]
  fn test_service_details() {
    let details = DingTalk::static_details();
    assert_eq!(details.service_name, "DingTalk");
    assert_eq!(details.protocols, vec!["dingtalk"]);
    assert!(!details.attachment_support);
  }
}
