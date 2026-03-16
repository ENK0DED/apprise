use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
pub struct FortySixElks {
  user: String,
  password: String,
  from_phone: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl FortySixElks {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Support https://user:pass@api.46elks.com/a1/sms?to=...&from=...
    let (user, password, from_phone) = if url.host.as_deref() == Some("api.46elks.com") {
      let u = url.user.clone().unwrap_or_default();
      let p = url.password.clone().unwrap_or_default();
      let from = url.get("from").unwrap_or("Apprise").to_string();
      (u, p, from)
    } else {
      let u = url.user.clone().unwrap_or_default();
      let p = url.password.clone().unwrap_or_default();
      let from = url.host.clone().unwrap_or_else(|| "Apprise".to_string());
      (u, p, from)
    };
    let mut targets = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Some(Self { user, password, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "46elks",
      service_url: Some("https://46elks.com"),
      setup_url: None,
      protocols: vec!["46elks", "elks"],
      description: "Send SMS via 46elks.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for FortySixElks {
  fn schemas(&self) -> &[&str] {
    &["46elks", "elks"]
  }
  fn service_name(&self) -> &str {
    "46elks"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
    let client = build_client(self.verify_certificate)?;
    let mut all_ok = true;
    for target in &self.targets {
      let params = [("to", target.as_str()), ("from", self.from_phone.as_str()), ("message", msg.as_str())];
      let resp =
        client.post("https://api.46elks.com/a1/sms").header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.password)).form(&params).send().await?;
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
      "46elks://",
      "46elks://user:pass@+15551234556",
      "46elks://user:pass@+15551234567/+46701234534?from=Acme",
      "elks://user:pass@+15551234123/",
      "46elks://user:pass@+15551234512",
      "46elks://user:pass@Acme/234512",
      "https://user1:pass@api.46elks.com/a1/sms?to=+15551234511&from=Acme",
      "46elks://user:pass@+15551234578",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  fn parse_elks(url: &str) -> FortySixElks {
    let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
    FortySixElks::from_url(&parsed).unwrap()
  }

  #[test]
  fn test_from_url_basic() {
    let e = parse_elks("46elks://user:pass@+15551234556");
    assert_eq!(e.user, "user");
    assert_eq!(e.password, "pass");
    // Host becomes from_phone in non-native URL mode
    assert_eq!(e.from_phone, "+15551234556");
    assert!(e.targets.is_empty());
  }

  #[test]
  fn test_from_url_with_targets() {
    let e = parse_elks("46elks://user:pass@+15551234567/+46701234534?from=Acme");
    assert_eq!(e.user, "user");
    assert_eq!(e.password, "pass");
    assert!(e.targets.contains(&"+46701234534".to_string()));
  }

  #[test]
  fn test_from_url_elks_schema() {
    let e = parse_elks("elks://user:pass@+15551234123/");
    assert_eq!(e.user, "user");
    assert_eq!(e.password, "pass");
  }

  #[test]
  fn test_from_url_native_url() {
    let e = parse_elks("https://user1:pass@api.46elks.com/a1/sms?to=%2B15551234511&from=Acme");
    assert_eq!(e.user, "user1");
    assert_eq!(e.password, "pass");
    assert_eq!(e.from_phone, "Acme");
    assert!(e.targets.contains(&"+15551234511".to_string()));
  }

  #[test]
  fn test_from_url_to_query() {
    let e = parse_elks("46elks://user:pass@+15551234567?to=+15559876543");
    assert!(e.targets.contains(&"+15559876543".to_string()));
  }

  #[test]
  fn test_service_details() {
    let details = FortySixElks::static_details();
    assert_eq!(details.service_name, "46elks");
    assert_eq!(details.protocols, vec!["46elks", "elks"]);
    assert!(!details.attachment_support);
  }
}
