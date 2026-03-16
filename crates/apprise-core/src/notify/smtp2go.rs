use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;

pub struct Smtp2Go {
  api_key: String,
  from: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Smtp2Go {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Require user@ for the from address
    let user = url.user.clone()?;
    if user.is_empty() {
      return None;
    }
    // Reject quotes in user
    if user.contains('"') {
      return None;
    }
    let api_key = url.host.clone()?;
    let from = url.get("from").unwrap_or("apprise@example.com").to_string();
    let targets: Vec<String> = url.path_parts.iter().map(|s| if s.contains('@') { s.clone() } else { format!("{}@example.com", s) }).collect();
    if targets.is_empty() {
      return None;
    }
    Some(Self { api_key, from, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SMTP2Go",
      service_url: Some("https://www.smtp2go.com"),
      setup_url: None,
      protocols: vec!["smtp2go"],
      description: "Send email via SMTP2Go API.",
      attachment_support: true,
    }
  }
}
#[async_trait]
impl Notify for Smtp2Go {
  fn schemas(&self) -> &[&str] {
    &["smtp2go"]
  }
  fn service_name(&self) -> &str {
    "SMTP2Go"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let mut payload = json!({ "api_key": self.api_key, "to": self.targets, "sender": self.from, "subject": ctx.title, "text_body": ctx.body });
    if !ctx.attachments.is_empty() {
      payload["attachments"] = json!(
        ctx
          .attachments
          .iter()
          .map(|att| json!({
              "filename": att.name,
              "fileblob": base64::engine::general_purpose::STANDARD.encode(&att.data),
              "mimetype": att.mime_type,
          }))
          .collect::<Vec<_>>()
      );
    }
    let resp = client.post("https://api.smtp2go.com/v3/email/send").header("User-Agent", APP_ID).json(&payload).send().await?;
    Ok(resp.status().is_success())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "smtp2go://",
      "smtp2go://:@/",
      // No token (no path parts = no targets)
      "smtp2go://user@localhost.localdomain",
      // Invalid from email address (quote in user)
      "smtp2go://\"@localhost.localdomain/aaaaaaaa",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // user@host/path (host is api_key, path is target)
      format!("smtp2go://user@apikey/test@example.com"),
      format!("smtp2go://user@apikey/test@example.com?format=markdown"),
      format!("smtp2go://user@apikey/test@example.com?format=html"),
      format!("smtp2go://user@apikey/test@example.com?format=text"),
      // bcc and cc (still need at least one path target)
      format!("smtp2go://user@apikey/test@example.com?bcc=user@example.com&cc=user2@example.com"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("smtp2go://user@myapikey/recipient@example.com").unwrap();
    let smtp = Smtp2Go::from_url(&parsed).unwrap();
    assert_eq!(smtp.api_key, "myapikey");
    assert!(smtp.targets.contains(&"recipient@example.com".to_string()));
  }

  #[test]
  fn test_from_url_no_user_returns_none() {
    // No user@ prefix
    let parsed = crate::utils::parse::ParsedUrl::parse("smtp2go://apikey/target@example.com").unwrap();
    // user is None => returns None
    assert!(Smtp2Go::from_url(&parsed).is_none());
  }

  #[test]
  fn test_from_url_user_with_quote_returns_none() {
    let parsed = crate::utils::parse::ParsedUrl::parse("smtp2go://us\"er@apikey/target@example.com").unwrap();
    assert!(Smtp2Go::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = Smtp2Go::static_details();
    assert_eq!(details.service_name, "SMTP2Go");
    assert_eq!(details.service_url, Some("https://www.smtp2go.com"));
    assert!(details.protocols.contains(&"smtp2go"));
    assert!(details.attachment_support);
  }

  #[test]
  fn test_from_url_non_email_target_gets_domain() {
    // Path parts without @ get @example.com appended
    let parsed = crate::utils::parse::ParsedUrl::parse("smtp2go://user@apikey/invalid").unwrap();
    let smtp = Smtp2Go::from_url(&parsed).unwrap();
    assert_eq!(smtp.targets[0], "invalid@example.com");
  }
}
