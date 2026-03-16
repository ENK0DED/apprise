use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct BulkVs {
  user: String,
  password: String,
  from_phone: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl BulkVs {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let user = url.get("user").map(|s| s.to_string()).or_else(|| url.user.clone())?;
    let password = url.get("password").map(|s| s.to_string()).or_else(|| url.password.clone())?;
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
      (url.host.clone()?, Vec::new())
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
    Some(Self { user, password, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "BulkVS",
      service_url: Some("https://bulkvs.com"),
      setup_url: None,
      protocols: vec!["bulkvs"],
      description: "Send SMS via BulkVS.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for BulkVs {
  fn schemas(&self) -> &[&str] {
    &["bulkvs"]
  }
  fn service_name(&self) -> &str {
    "BulkVS"
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
      let payload = json!({ "from": self.from_phone, "to": [target], "body": msg });
      let resp = client
        .post("https://portal.bulkvs.com/api/v1.0/messageSend")
        .header("User-Agent", APP_ID)
        .basic_auth(&self.user, Some(&self.password))
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

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "bulkvs://uuuuuuuuuu:pppppppppp@2222222222",
      "bulkvs://aaaaa:bbbbbbbbbb@9876543210/33333333333/abcd/",
      "bulkvs://bbbbb:cccccccccc@44444444444?batch=y",
      "bulkvs://aaaaaaaaaa:bbbbbbbbbb@55555555555",
      "bulkvs://?user=zzzzzzzzzz&password=yyyyyyyyyy&from=55555555555",
      "bulkvs://?user=aaaaaaaaaa&password=bbbbbbbbbb&from=55555555555&to=7777777777777",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "bulkvs://",
      "bulkvs://:@/",
      // Just user, no password
      "bulkvs://aaaaaaaaaa@9876543210/",
      // Invalid source number (too short)
      "bulkvs://uuuuuuuuuu:pppppppppp@33333",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_struct_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("bulkvs://myuser:mypass@14051231234/15551231234/15555555555").unwrap();
    let obj = BulkVs::from_url(&parsed).unwrap();
    assert_eq!(obj.user, "myuser");
    assert_eq!(obj.password, "mypass");
    assert_eq!(obj.from_phone, "14051231234");
    assert_eq!(obj.targets.len(), 2);
    assert!(obj.targets.contains(&"15551231234".to_string()));
    assert!(obj.targets.contains(&"15555555555".to_string()));
  }

  #[test]
  fn test_from_url_query_params() {
    let parsed = crate::utils::parse::ParsedUrl::parse("bulkvs://?user=testuser&password=testpass&from=55555555555&to=66666666666").unwrap();
    let obj = BulkVs::from_url(&parsed).unwrap();
    assert_eq!(obj.user, "testuser");
    assert_eq!(obj.password, "testpass");
    assert_eq!(obj.from_phone, "55555555555");
    assert!(obj.targets.contains(&"66666666666".to_string()));
  }

  #[test]
  fn test_from_phone_too_short() {
    let parsed = crate::utils::parse::ParsedUrl::parse("bulkvs://user:pass@33333").unwrap();
    assert!(BulkVs::from_url(&parsed).is_none());
  }

  #[test]
  fn test_service_details() {
    let details = BulkVs::static_details();
    assert_eq!(details.service_name, "BulkVS");
    assert_eq!(details.protocols, vec!["bulkvs"]);
    assert!(!details.attachment_support);
  }
}
