use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct GoogleChat {
  workspace: String,
  webhook_key: String,
  webhook_token: String,
  thread_key: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl GoogleChat {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // gchat://workspace/webhook_key/webhook_token
    // or gchat://?workspace=ws&key=mykey&token=mytoken
    let workspace = url.host.clone().filter(|h| !h.is_empty()).or_else(|| url.get("workspace").map(|s| s.to_string()))?;
    let webhook_key = url.path_parts.first().cloned().or_else(|| url.get("key").map(|s| s.to_string()))?;
    let webhook_token = url.path_parts.get(1).cloned().or_else(|| url.get("token").map(|s| s.to_string()))?;
    let thread_key = url.get("thread").map(|s| s.to_string());
    Some(Self { workspace, webhook_key, webhook_token, thread_key, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Google Chat",
      service_url: Some("https://chat.google.com"),
      setup_url: None,
      protocols: vec!["gchat"],
      description: "Send via Google Chat webhooks.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for GoogleChat {
  fn schemas(&self) -> &[&str] {
    &["gchat"]
  }
  fn service_name(&self) -> &str {
    "Google Chat"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let mut url = format!("https://chat.googleapis.com/v1/spaces/{}/messages?key={}&token={}", self.workspace, self.webhook_key, self.webhook_token);
    if let Some(ref tk) = self.thread_key {
      url = format!("{}&threadKey={}&messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD", url, urlencoding::encode(tk));
    }
    let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("*{}*\n{}", ctx.title, ctx.body) };
    let payload = json!({ "text": text });
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
      "gchat://workspace/key/token",
      "gchat://?workspace=ws&key=mykey&token=mytoken",
      "gchat://?workspace=ws&key=mykey&token=mytoken&thread=abc123",
      "gchat://?workspace=ws&key=mykey&token=mytoken&threadKey=abc345",
      "https://chat.googleapis.com/v1/spaces/myworkspace/messages?key=mykey&token=mytoken",
      "https://chat.googleapis.com/v1/spaces/myworkspace/messages?key=mykey&token=mytoken&threadKey=mythreadkey",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["gchat://", "gchat://:@/", "gchat://workspace", "gchat://workspace/key/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn parse_gchat(url: &str) -> GoogleChat {
    let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
    GoogleChat::from_url(&parsed).unwrap()
  }

  #[test]
  fn test_from_url_path_style() {
    let g = parse_gchat("gchat://workspace/key/token");
    assert_eq!(g.workspace, "workspace");
    assert_eq!(g.webhook_key, "key");
    assert_eq!(g.webhook_token, "token");
    assert_eq!(g.thread_key, None);
  }

  #[test]
  fn test_from_url_query_style() {
    let g = parse_gchat("gchat://?workspace=ws&key=mykey&token=mytoken");
    assert_eq!(g.workspace, "ws");
    assert_eq!(g.webhook_key, "mykey");
    assert_eq!(g.webhook_token, "mytoken");
  }

  #[test]
  fn test_from_url_with_thread_key() {
    let g = parse_gchat("gchat://?workspace=ws&key=mykey&token=mytoken&thread=abc123");
    assert_eq!(g.thread_key, Some("abc123".to_string()));
  }

  #[test]
  fn test_from_url_no_workspace_returns_none() {
    let parsed = crate::utils::parse::ParsedUrl::parse("gchat://").unwrap();
    assert!(GoogleChat::from_url(&parsed).is_none());
  }

  #[test]
  fn test_from_url_no_key_returns_none() {
    let parsed = crate::utils::parse::ParsedUrl::parse("gchat://workspace").unwrap();
    assert!(GoogleChat::from_url(&parsed).is_none());
  }

  #[test]
  fn test_from_url_no_token_returns_none() {
    let parsed = crate::utils::parse::ParsedUrl::parse("gchat://workspace/key/").unwrap();
    assert!(GoogleChat::from_url(&parsed).is_none());
  }

  #[test]
  fn test_service_details() {
    let details = GoogleChat::static_details();
    assert_eq!(details.service_name, "Google Chat");
    assert_eq!(details.protocols, vec!["gchat"]);
    assert!(!details.attachment_support);
  }
}
