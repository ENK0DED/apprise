use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct HttpSms {
  apikey: String,
  from_phone: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl HttpSms {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url.get("key").map(|s| s.to_string()).or_else(|| url.user.clone())?;
    if apikey.is_empty() {
      return None;
    }
    // If ?from= is set, host becomes a target; otherwise host is from_phone
    let (from_phone, mut targets) = if let Some(from) = url.get("from").or_else(|| url.get("source")) {
      let mut t = Vec::new();
      if let Some(h) = url.host.as_deref() {
        if !h.is_empty() && h != "_" {
          t.push(h.to_string());
        }
      }
      (from.to_string(), t)
    } else {
      (url.host.clone().unwrap_or_default(), Vec::new())
    };
    if from_phone.is_empty() || from_phone == "_" {
      return None;
    }
    // Validate from_phone (must be 10-14 digits)
    let digits: String = from_phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 10 || digits.len() > 14 {
      return None;
    }
    targets.extend(url.path_parts.clone());
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Some(Self { apikey, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "HttpSMS",
      service_url: Some("https://httpsms.com"),
      setup_url: None,
      protocols: vec!["httpsms"],
      description: "Send SMS via HttpSMS.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for HttpSms {
  fn schemas(&self) -> &[&str] {
    &["httpsms"]
  }
  fn service_name(&self) -> &str {
    "HttpSMS"
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
      let payload = json!({ "content": msg, "from": self.from_phone, "to": target });
      let resp = client
        .post("https://api.httpsms.com/v1/messages/send")
        .header("User-Agent", APP_ID)
        .header("x-api-key", self.apikey.as_str())
        .json(&payload)
        .send()
        .await?;
      if !resp.status().is_success() && resp.status().as_u16() != 200 && resp.status().as_u16() != 202 {
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

  fn parse_httpsms(url: &str) -> Option<HttpSms> {
    ParsedUrl::parse(url).and_then(|p| HttpSms::from_url(&p))
  }

  #[test]
  fn test_invalid_urls() {
    let short_source = format!("httpsms://{}:{}@{}", "u".repeat(10), "p".repeat(10), "3".repeat(5));
    let urls: Vec<&str> = vec![
      "httpsms://",
      "httpsms://:@/",
      // invalid source number (too short)
      &short_source,
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // apikey@from (default to self)
      format!("httpsms://{}@{}", "p".repeat(10), "2".repeat(10)),
      // apikey@from/target with valid target
      format!("httpsms://{}@{}/{}", "b".repeat(10), "9876543210", "3".repeat(11)),
      // apikey@from with 11-digit from
      format!("httpsms://{}@{}", "c".repeat(10), "4".repeat(11)),
      format!("httpsms://{}@{}", "b".repeat(10), "5".repeat(11)),
      // use get args
      format!("httpsms://?key={}&from={}", "y".repeat(10), "5".repeat(11)),
      format!("httpsms://?key={}&from={}&to={}", "b".repeat(10), "5".repeat(11), "7".repeat(13)),
    ];
    for url in &urls {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_target_still_parses() {
    // Invalid target number provided -- plugin still parses but notify would fail
    let url = format!("httpsms://{}@{}/{}", "p".repeat(10), "1".repeat(10), "55");
    let obj = parse_httpsms(&url);
    assert!(obj.is_some(), "Should parse even with short target");
  }

  #[test]
  fn test_from_url_fields() {
    let url = format!("httpsms://{}@9876543210/{}/abcd/", "b".repeat(10), "3".repeat(11));
    let obj = parse_httpsms(&url).unwrap();
    assert_eq!(obj.apikey, "b".repeat(10));
    assert_eq!(obj.from_phone, "9876543210");
    // "abcd" is not filtered by from_url (it's just a path part),
    // but 33333333333 should be present
    assert!(obj.targets.contains(&"3".repeat(11)));
  }

  #[test]
  fn test_query_param_key_and_from() {
    let url = format!("httpsms://?key={}&from={}", "y".repeat(10), "5".repeat(11));
    let obj = parse_httpsms(&url).unwrap();
    assert_eq!(obj.apikey, "y".repeat(10));
    assert_eq!(obj.from_phone, "5".repeat(11));
  }

  #[test]
  fn test_query_param_to() {
    let url = format!("httpsms://?key={}&from={}&to={}", "b".repeat(10), "5".repeat(11), "7".repeat(13));
    let obj = parse_httpsms(&url).unwrap();
    assert!(obj.targets.contains(&"7".repeat(13)));
  }

  #[test]
  fn test_service_details() {
    let details = HttpSms::static_details();
    assert_eq!(details.service_name, "HttpSMS");
    assert_eq!(details.protocols, vec!["httpsms"]);
  }
}
