use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

pub struct Threema {
  gateway_id: String,
  secret: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Threema {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // threema://gateway_id:secret/target or threema://*GWID@secret/target
    // or threema:///?secret=secret&from=*THEGWID&to=...
    let gateway_id = url.user.clone().or_else(|| url.get("from").map(|s| s.to_string())).or_else(|| url.get("gwid").map(|s| s.to_string()))?;
    // Gateway ID must start with *
    if !gateway_id.starts_with('*') {
      return None;
    }
    let secret =
      url.password.clone().or_else(|| url.host.clone().filter(|h| !h.is_empty() && h != "_")).or_else(|| url.get("secret").map(|s| s.to_string()))?;
    if secret.is_empty() {
      return None;
    }
    let mut targets = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Some(Self { gateway_id, secret, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Threema Gateway",
      service_url: Some("https://gateway.threema.ch"),
      setup_url: None,
      protocols: vec!["threema"],
      description: "Send messages via Threema Gateway.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Threema {
  fn schemas(&self) -> &[&str] {
    &["threema"]
  }
  fn service_name(&self) -> &str {
    "Threema Gateway"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
    let mut all_ok = true;
    for target in &self.targets {
      let params = [("from", self.gateway_id.as_str()), ("to", target.as_str()), ("secret", self.secret.as_str()), ("text", msg.as_str())];
      let resp = client.post("https://msgapi.threema.ch/send_simple").header("User-Agent", APP_ID).form(&params).send().await?;
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

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "threema://*THEGWID@secret",
      "threema://*THEGWID@secret/16134443333",
      "threema://*THEGWID@secret/16134442222/16134443333",
      "threema:///?secret=secret&from=*THEGWID&to=16134448888,user1@gmail.com,abcd1234",
      "threema:///?secret=secret&gwid=*THEGWID&to=16134448888,user2@gmail.com,abcd1234",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "threema://",
      "threema://@:",
      // gateway id must start with *
      "threema://user@secret",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_user_at_host() {
    let parsed = crate::utils::parse::ParsedUrl::parse("threema://*THEGWID@secret/16134443333").unwrap();
    let t = Threema::from_url(&parsed).unwrap();
    assert_eq!(t.gateway_id, "*THEGWID");
    assert_eq!(t.secret, "secret");
    assert_eq!(t.targets, vec!["16134443333"]);
  }

  #[test]
  fn test_from_url_multiple_targets() {
    let parsed = crate::utils::parse::ParsedUrl::parse("threema://*THEGWID@secret/16134442222/16134443333").unwrap();
    let t = Threema::from_url(&parsed).unwrap();
    assert_eq!(t.targets.len(), 2);
    assert!(t.targets.contains(&"16134442222".to_string()));
    assert!(t.targets.contains(&"16134443333".to_string()));
  }

  #[test]
  fn test_from_url_query_params_from() {
    let parsed = crate::utils::parse::ParsedUrl::parse("threema:///?secret=mysecret&from=*THEGWID&to=16134448888").unwrap();
    let t = Threema::from_url(&parsed).unwrap();
    assert_eq!(t.gateway_id, "*THEGWID");
    assert_eq!(t.secret, "mysecret");
    assert!(t.targets.contains(&"16134448888".to_string()));
  }

  #[test]
  fn test_from_url_query_params_gwid() {
    let parsed = crate::utils::parse::ParsedUrl::parse("threema:///?secret=mysecret&gwid=*THEGWID&to=16134448888").unwrap();
    let t = Threema::from_url(&parsed).unwrap();
    assert_eq!(t.gateway_id, "*THEGWID");
  }

  #[test]
  fn test_from_url_no_targets() {
    let parsed = crate::utils::parse::ParsedUrl::parse("threema://*THEGWID@secret").unwrap();
    let t = Threema::from_url(&parsed).unwrap();
    assert!(t.targets.is_empty());
  }

  #[test]
  fn test_gateway_must_start_with_star() {
    let parsed = crate::utils::parse::ParsedUrl::parse("threema://NOLEADINGSTAR@secret/16134443333").unwrap();
    assert!(Threema::from_url(&parsed).is_none());
  }

  #[test]
  fn test_from_url_to_param_multiple() {
    let parsed = crate::utils::parse::ParsedUrl::parse("threema:///?secret=sec&from=*GW&to=111,222,333").unwrap();
    let t = Threema::from_url(&parsed).unwrap();
    assert_eq!(t.targets.len(), 3);
  }

  #[test]
  fn test_static_details() {
    let details = Threema::static_details();
    assert_eq!(details.service_name, "Threema Gateway");
    assert_eq!(details.service_url, Some("https://gateway.threema.ch"));
    assert!(details.protocols.contains(&"threema"));
    assert!(!details.attachment_support);
  }
}
