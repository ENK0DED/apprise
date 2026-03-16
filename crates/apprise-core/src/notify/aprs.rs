use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

pub struct Aprs {
  user: String,
  password: String,
  targets: Vec<String>,
  tags: Vec<String>,
}
impl Aprs {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let user = url.user.clone()?;
    let password = url.password.clone()?;
    let targets = url.path_parts.clone();
    if targets.is_empty() {
      return None;
    }
    Some(Self { user, password, targets, tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "APRS",
      service_url: Some("https://www.aprs.org"),
      setup_url: None,
      protocols: vec!["aprs"],
      description: "Send messages via APRS (Amateur Radio).",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Aprs {
  fn schemas(&self) -> &[&str] {
    &["aprs"]
  }
  fn service_name(&self) -> &str {
    "APRS"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
    let mut stream = TcpStream::connect("rotate.aprs2.net:10152").await.map_err(|e| NotifyError::Other(e.to_string()))?;
    let login = format!("user {} pass {} vers Apprise 1.9.8\r\n", self.user, self.password);
    stream.write_all(login.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
    for target in &self.targets {
      let aprs_msg = format!("{}>{}>APRS::{}:{}\r\n", self.user, self.user, target, &msg[..msg.len().min(67)]);
      stream.write_all(aprs_msg.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
    }
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::utils::parse::ParsedUrl;

  #[test]
  fn test_valid_urls() {
    let valid_urls = vec!["aprs://user:pass@localhost/target1", "aprs://user:pass@localhost/target1/target2"];
    for url in &valid_urls {
      let parsed = ParsedUrl::parse(url);
      assert!(parsed.is_some(), "ParsedUrl::parse failed for: {}", url);
      let parsed = parsed.unwrap();
      assert!(Aprs::from_url(&parsed).is_some(), "Aprs::from_url returned None for valid URL: {}", url,);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let invalid_urls = vec!["aprs://user:pass@localhost", "aprs://localhost/target", "aprs://:@localhost/target"];
    for url in &invalid_urls {
      let result = ParsedUrl::parse(url).and_then(|p| Aprs::from_url(&p));
      assert!(result.is_none(), "Aprs::from_url should return None for: {}", url,);
    }
  }

  #[test]
  fn test_aprs_struct_fields() {
    let parsed = ParsedUrl::parse("aprs://DF1JSL-15:12345@DF1ABC/DF1XYZ").unwrap();
    let aprs = Aprs::from_url(&parsed).unwrap();
    assert_eq!(aprs.user, "DF1JSL-15");
    assert_eq!(aprs.password, "12345");
    assert!(aprs.targets.contains(&"DF1ABC".to_string()) || aprs.targets.contains(&"DF1XYZ".to_string()));
  }

  #[test]
  fn test_aprs_multiple_targets() {
    let parsed = ParsedUrl::parse("aprs://user:pass@localhost/target1/target2/target3").unwrap();
    let aprs = Aprs::from_url(&parsed).unwrap();
    assert_eq!(aprs.targets.len(), 3);
  }

  #[test]
  fn test_aprs_no_password_fails() {
    let result = ParsedUrl::parse("aprs://user@localhost/target").and_then(|p| Aprs::from_url(&p));
    assert!(result.is_none(), "APRS without password should fail");
  }

  #[test]
  fn test_aprs_no_targets_fails() {
    let result = ParsedUrl::parse("aprs://user:pass@localhost").and_then(|p| Aprs::from_url(&p));
    assert!(result.is_none(), "APRS without targets should fail");
  }

  #[test]
  fn test_aprs_static_details() {
    let details = Aprs::static_details();
    assert_eq!(details.service_name, "APRS");
    assert_eq!(details.protocols, vec!["aprs"]);
    assert!(!details.attachment_support);
  }
}
