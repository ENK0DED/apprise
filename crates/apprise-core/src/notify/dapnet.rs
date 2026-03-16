use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct Dapnet {
  user: String,
  password: String,
  targets: Vec<String>,
  txgroups: Vec<String>,
  priority: i32,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Dapnet {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let user = url.user.clone()?;
    let password = url.password.clone()?;
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
    let txgroups: Vec<String> = url.get("txgroups").map(|s| s.split(',').map(|g| g.trim().to_string()).collect()).unwrap_or_else(|| vec!["dl-all".to_string()]);
    let priority = url.get("priority").and_then(|p| p.parse().ok()).unwrap_or(0);
    Some(Self { user, password, targets, txgroups, priority, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "DAPNET",
      service_url: Some("https://hampager.de"),
      setup_url: None,
      protocols: vec!["dapnet"],
      description: "Send pager messages via DAPNET.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Dapnet {
  fn schemas(&self) -> &[&str] {
    &["dapnet"]
  }
  fn service_name(&self) -> &str {
    "DAPNET"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
    let payload = json!({ "text": msg, "callSignNames": self.targets, "transmitterGroupNames": self.txgroups, "emergency": self.priority >= 1 });
    let client = build_client(self.verify_certificate)?;
    let resp =
      client.post("http://www.hampager.de:8080/calls").header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.password)).json(&payload).send().await?;
    if resp.status().is_success() || resp.status().as_u16() == 201 {
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
      "dapnet://user:pass@DF1ABC",
      "dapnet://user:pass@DF1ABC/DF1DEF",
      "dapnet://user:pass@DF1ABC-1/DF1ABC/DF1ABC-15",
      "dapnet://user:pass@?to=DF1ABC,DF1DEF",
      "dapnet://user:pass@DF1ABC?priority=normal",
      "dapnet://user:pass@DF1ABC/0A1DEF?priority=em&batch=false",
      "dapnet://user:pass@DF1ABC?priority=invalid",
      "dapnet://user:pass@DF1ABC?txgroups=dl-all,all",
      "dapnet://user:pass@DF1ABC?txgroups=invalid",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["dapnet://", "dapnet://:@/", "dapnet://user:pass", "dapnet://user@host"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn parse_dapnet(url: &str) -> Dapnet {
    let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
    Dapnet::from_url(&parsed).unwrap()
  }

  #[test]
  fn test_from_url_single_target() {
    let d = parse_dapnet("dapnet://user:pass@DF1ABC");
    assert_eq!(d.user, "user");
    assert_eq!(d.password, "pass");
    assert_eq!(d.targets, vec!["DF1ABC"]);
    assert_eq!(d.txgroups, vec!["dl-all"]);
    assert_eq!(d.priority, 0);
  }

  #[test]
  fn test_from_url_multiple_targets() {
    let d = parse_dapnet("dapnet://user:pass@DF1ABC/DF1DEF");
    assert_eq!(d.targets, vec!["DF1ABC", "DF1DEF"]);
  }

  #[test]
  fn test_from_url_to_query_param() {
    let d = parse_dapnet("dapnet://user:pass@?to=DF1ABC,DF1DEF");
    assert!(d.targets.contains(&"DF1ABC".to_string()));
    assert!(d.targets.contains(&"DF1DEF".to_string()));
  }

  #[test]
  fn test_from_url_custom_txgroups() {
    let d = parse_dapnet("dapnet://user:pass@DF1ABC?txgroups=dl-all,all");
    assert_eq!(d.txgroups, vec!["dl-all", "all"]);
  }

  #[test]
  fn test_from_url_priority() {
    // priority=1 should parse as emergency
    let d = parse_dapnet("dapnet://user:pass@DF1ABC?priority=1");
    assert_eq!(d.priority, 1);
  }

  #[test]
  fn test_from_url_invalid_priority_defaults_to_zero() {
    let d = parse_dapnet("dapnet://user:pass@DF1ABC?priority=invalid");
    assert_eq!(d.priority, 0);
  }

  #[test]
  fn test_no_targets_returns_none() {
    let parsed = crate::utils::parse::ParsedUrl::parse("dapnet://user:pass@").unwrap();
    assert!(Dapnet::from_url(&parsed).is_none());
  }

  #[test]
  fn test_service_details() {
    let details = Dapnet::static_details();
    assert_eq!(details.service_name, "DAPNET");
    assert_eq!(details.protocols, vec!["dapnet"]);
    assert!(!details.attachment_support);
  }
}
