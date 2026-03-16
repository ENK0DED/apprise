use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
pub struct SmsManager {
  apikey: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl SmsManager {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Validate gateway if provided
    if let Some(gw) = url.get("gateway") {
      let g = gw.to_lowercase();
      if !["economy", "low", "high", "standard"].contains(&g.as_str()) {
        return None;
      }
    }
    let apikey = url.get("key").map(|s| s.to_string()).or_else(|| url.user.clone())?;
    if apikey.is_empty() {
      return None;
    }
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
    Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SmsManager",
      service_url: Some("https://smsmanager.cz"),
      setup_url: None,
      protocols: vec!["smsmanager", "smsmgr"],
      description: "Send SMS via SmsManager (CZ).",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for SmsManager {
  fn schemas(&self) -> &[&str] {
    &["smsmanager"]
  }
  fn service_name(&self) -> &str {
    "SmsManager"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
    let client = build_client(self.verify_certificate)?;
    let mut all_ok = true;
    for target in &self.targets {
      let params = [("apikey", self.apikey.as_str()), ("number", target.as_str()), ("message", msg.as_str()), ("type", "promotional")];
      let resp = client.post("https://http-api.smsmanager.cz/Send").header("User-Agent", APP_ID).form(&params).send().await?;
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
  fn test_invalid_urls() {
    let urls = vec![
      "smsmgr://",
      "smsmgr://:@/",
      // invalid gateway
      "smsmgr://aaaaaaaaaa@11111111111?gateway=invalid",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // apikey@phone with invalid number filtered but valid one present
      "smsmgr://zzzzzzzzzz@123/33333333333/abcd/+44444444444",
      // batch mode
      "smsmgr://bbbbb@44444444444?batch=y",
      // gateway=low
      "smsmgr://aaaaaaaaaa@11111111111?gateway=low",
      // use get args: key= and from=
      "smsmgr://11111111111?key=aaaaaaaaaa&from=user",
      // use get args: to=, key=, sender=
      "smsmgr://_?to=11111111111,22222222222&key=bbbbbbbbbb&sender=5555555555555",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("smsmgr://myapikey@15551231234/15555555555").unwrap();
    let sms = SmsManager::from_url(&parsed).unwrap();
    assert_eq!(sms.apikey, "myapikey");
    assert_eq!(sms.targets.len(), 2);
    assert!(sms.targets.contains(&"15551231234".to_string()));
    assert!(sms.targets.contains(&"15555555555".to_string()));
  }

  #[test]
  fn test_from_url_with_key_param() {
    let parsed = crate::utils::parse::ParsedUrl::parse("smsmgr://11111111111?key=testapikey").unwrap();
    let sms = SmsManager::from_url(&parsed).unwrap();
    assert_eq!(sms.apikey, "testapikey");
    assert!(sms.targets.contains(&"11111111111".to_string()));
  }

  #[test]
  fn test_from_url_with_to_param() {
    let parsed = crate::utils::parse::ParsedUrl::parse("smsmgr://_?to=11111111111,22222222222&key=bbbbbbbbbb").unwrap();
    let sms = SmsManager::from_url(&parsed).unwrap();
    assert_eq!(sms.apikey, "bbbbbbbbbb");
    assert_eq!(sms.targets.len(), 2);
  }

  #[test]
  fn test_gateway_validation() {
    // Valid gateways
    for gw in &["economy", "low", "high", "standard"] {
      let url = format!("smsmgr://apikey@11111111111?gateway={}", gw);
      let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
      assert!(SmsManager::from_url(&parsed).is_some(), "Gateway {} should be valid", gw);
    }
    // Invalid gateway
    let parsed = crate::utils::parse::ParsedUrl::parse("smsmgr://apikey@11111111111?gateway=invalid").unwrap();
    assert!(SmsManager::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = SmsManager::static_details();
    assert_eq!(details.service_name, "SmsManager");
    assert_eq!(details.service_url, Some("https://smsmanager.cz"));
    assert!(details.protocols.contains(&"smsmanager"));
    assert!(details.protocols.contains(&"smsmgr"));
    assert!(!details.attachment_support);
  }

  #[test]
  fn test_no_targets_returns_none() {
    // apikey present but no valid target
    let parsed = crate::utils::parse::ParsedUrl::parse("smsmgr://apikey@_").unwrap();
    // _ is filtered out as host, and no path_parts => None
    assert!(SmsManager::from_url(&parsed).is_none());
  }
}
