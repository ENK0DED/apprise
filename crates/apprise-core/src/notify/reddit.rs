use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::Value;

pub struct Reddit {
  app_id: String,
  app_secret: String,
  user: String,
  password: String,
  subreddits: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Reddit {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // reddit://app_id:app_secret@user:password/subreddit1/subreddit2
    // or reddit://?user=u&pass=p&app_id=id&app_secret=s&to=sub1,sub2
    // If user@host format was used, the host must be valid (non-empty)
    let has_userinfo = url.raw.contains('@') && url.user.is_some();
    if has_userinfo {
      if let Some(ref h) = url.host {
        if h.trim().is_empty() {
          return None;
        }
      } else {
        return None;
      }
    }

    let app_id = url.user.clone().or_else(|| url.get("app_id").map(|s| s.to_string()))?;
    // Validate app_id (reject invalid percent-encoding)
    if app_id.contains('%') {
      return None;
    }
    let app_secret = url.password.clone().or_else(|| url.get("app_secret").map(|s| s.to_string()))?;

    let (user, password) = if let Some(ref h) = url.host {
      // Reject invalid percent-encoding in host
      if h.contains('%') {
        let decoded = urlencoding::decode(h).unwrap_or_default().into_owned();
        if decoded.contains('%') || decoded == *h {
          return None;
        }
      }
      if h.contains(':') {
        let parts: Vec<&str> = h.splitn(2, ':').collect();
        (parts[0].to_string(), parts.get(1).unwrap_or(&"").to_string())
      } else {
        let u = url.get("user").map(|s| s.to_string()).unwrap_or_else(|| h.clone());
        let p = url.get("pass").map(|s| s.to_string()).unwrap_or_default();
        (u, p)
      }
    } else {
      let u = url.get("user").map(|s| s.to_string())?;
      let p = url.get("pass").map(|s| s.to_string()).unwrap_or_default();
      (u, p)
    };

    let mut subreddits = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      subreddits.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    if subreddits.is_empty() {
      return None;
    }
    // Validate kind if provided
    if let Some(kind) = url.get("kind") {
      match kind.to_lowercase().as_str() {
        "auto" | "self" | "link" | "" => {}
        _ => return None,
      }
    }
    Some(Self { app_id, app_secret, user, password, subreddits, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Reddit",
      service_url: Some("https://reddit.com"),
      setup_url: None,
      protocols: vec!["reddit"],
      description: "Post to Reddit subreddits.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Reddit {
  fn schemas(&self) -> &[&str] {
    &["reddit"]
  }
  fn service_name(&self) -> &str {
    "Reddit"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    // Get OAuth token
    let token_params = [("grant_type", "password"), ("username", self.user.as_str()), ("password", self.password.as_str())];
    let token_resp = client
      .post("https://www.reddit.com/api/v1/access_token")
      .header("User-Agent", APP_ID)
      .basic_auth(&self.app_id, Some(&self.app_secret))
      .form(&token_params)
      .send()
      .await?;
    let token_json: Value = token_resp.json().await.map_err(|e| NotifyError::Other(e.to_string()))?;
    let access_token = token_json["access_token"].as_str().ok_or_else(|| NotifyError::Auth("No access token".into()))?;

    let mut all_ok = true;
    for sub in &self.subreddits {
      let params = [("sr", sub.as_str()), ("kind", "self"), ("title", ctx.title.as_str()), ("text", ctx.body.as_str()), ("resubmit", "true")];
      let resp = client
        .post("https://oauth.reddit.com/api/submit")
        .header("User-Agent", APP_ID)
        .header("Authorization", format!("Bearer {}", access_token))
        .form(&params)
        .send()
        .await?;
      if !resp.status().is_success() {
        all_ok = false;
      }
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::utils::parse::ParsedUrl;

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "reddit://user:password@app-id/app-secret/apprise",
      "reddit://user:password@app-id/app-secret",
      "reddit://user:password@app-id/app-secret/apprise/subreddit2",
      "reddit://user:pass@id/secret/sub/?ad=yes&nsfw=yes&replies=no&resubmit=yes&spoiler=yes&kind=self",
      "reddit://?user=l2g&pass=pass&app_secret=abc123&app_id=54321&to=sub1,sub2",
      "reddit://user:pass@id/secret/sub7/sub6/sub5/?flair_id=wonder&flair_text=not%20for%20you",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "reddit://",
      "reddit://:@/",
      "reddit://user@app_id/app_secret/",
      "reddit://user:password@app_id/",
      "reddit://user:password@app%id/appsecret/apprise",
      "reddit://user:password@app%id/app_secret/apprise",
      "reddit://user:password@app-id/app-secret/apprise?kind=invalid",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields_basic() {
    let parsed = ParsedUrl::parse("reddit://user:password@app-id/app-secret/apprise").unwrap();
    let r = Reddit::from_url(&parsed).unwrap();
    assert_eq!(r.app_id, "user");
    assert_eq!(r.app_secret, "password");
    assert!(r.subreddits.contains(&"apprise".to_string()));
  }

  #[test]
  fn test_from_url_multiple_subreddits() {
    let parsed = ParsedUrl::parse("reddit://user:password@app-id/app-secret/apprise/subreddit2").unwrap();
    let r = Reddit::from_url(&parsed).unwrap();
    assert!(r.subreddits.len() >= 2);
    assert!(r.subreddits.contains(&"apprise".to_string()));
    assert!(r.subreddits.contains(&"subreddit2".to_string()));
  }

  #[test]
  fn test_from_url_query_params() {
    let parsed = ParsedUrl::parse("reddit://?user=l2g&pass=pass&app_secret=abc123&app_id=54321&to=sub1,sub2").unwrap();
    let r = Reddit::from_url(&parsed).unwrap();
    assert_eq!(r.app_id, "54321");
    assert_eq!(r.app_secret, "abc123");
    assert_eq!(r.user, "l2g");
    assert!(r.subreddits.contains(&"sub1".to_string()));
    assert!(r.subreddits.contains(&"sub2".to_string()));
  }

  #[test]
  fn test_kind_validation() {
    // Valid kinds
    for kind in &["self", "link", "auto"] {
      let url = format!("reddit://user:pass@id/secret/sub?kind={}", kind);
      let parsed = ParsedUrl::parse(&url).unwrap();
      assert!(Reddit::from_url(&parsed).is_some(), "Kind {} should be valid", kind);
    }
    // Invalid kind
    let parsed = ParsedUrl::parse("reddit://user:pass@id/secret/sub?kind=invalid").unwrap();
    assert!(Reddit::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = Reddit::static_details();
    assert_eq!(details.service_name, "Reddit");
    assert_eq!(details.service_url, Some("https://reddit.com"));
    assert!(details.protocols.contains(&"reddit"));
    assert!(!details.attachment_support);
  }

  #[test]
  fn test_percent_in_app_id_rejected() {
    let parsed = ParsedUrl::parse("reddit://user:password@app%id/appsecret/apprise").unwrap();
    assert!(Reddit::from_url(&parsed).is_none());
  }
}
