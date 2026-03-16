use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct Line {
  token: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Line {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = url.host.clone().filter(|h| !h.is_empty()).or_else(|| url.get("token").map(|s| s.to_string()))?;
    // Decode and validate
    let decoded = urlencoding::decode(&token).unwrap_or_default().into_owned();
    if decoded.trim().is_empty() {
      return None;
    }
    let mut targets = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "LINE",
      service_url: Some("https://line.me"),
      setup_url: None,
      protocols: vec!["line"],
      description: "Send LINE messages via bot.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Line {
  fn schemas(&self) -> &[&str] {
    &["line"]
  }
  fn service_name(&self) -> &str {
    "LINE"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
    let client = build_client(self.verify_certificate)?;
    let mut all_ok = true;
    for target in &self.targets {
      let payload = json!({ "to": target, "messages": [{ "type": "text", "text": text }] });
      let resp = client
        .post("https://api.line.me/v2/bot/message/push")
        .header("User-Agent", APP_ID)
        .header("Authorization", format!("Bearer {}", self.token))
        .json(&payload)
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

  fn parse_line(url: &str) -> Option<Line> {
    ParsedUrl::parse(url).and_then(|p| Line::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "line://token",
      "line://token=/target",
      "line://token/target?image=no",
      "line://a/very/long/token=/target?image=no",
      "line://?token=token&to=target1",
      "line://token/target",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["line://", "line://%20/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_token_no_target() {
    let obj = parse_line("line://token").unwrap();
    assert_eq!(obj.token, "token");
    assert!(obj.targets.is_empty());
  }

  #[test]
  fn test_from_url_token_with_target() {
    let obj = parse_line("line://token=/target").unwrap();
    // token= is the host, target is path part
    assert!(obj.targets.contains(&"target".to_string()));
  }

  #[test]
  fn test_from_url_query_params() {
    let obj = parse_line("line://?token=token&to=target1").unwrap();
    assert_eq!(obj.token, "token");
    assert!(obj.targets.contains(&"target1".to_string()));
  }

  #[test]
  fn test_multiple_targets_via_path() {
    let obj = parse_line("line://token/target1/target2").unwrap();
    assert_eq!(obj.targets.len(), 2);
    assert!(obj.targets.contains(&"target1".to_string()));
    assert!(obj.targets.contains(&"target2".to_string()));
  }

  #[test]
  fn test_service_details() {
    let details = Line::static_details();
    assert_eq!(details.service_name, "LINE");
    assert_eq!(details.protocols, vec!["line"]);
  }
}
