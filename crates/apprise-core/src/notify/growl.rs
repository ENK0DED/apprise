use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Growl {
  host: String,
  port: u16,
  password: Option<String>,
  tags: Vec<String>,
}

impl Growl {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;
    let port = url.port.unwrap_or(23053);
    Some(Self { host, port, password: url.password.clone(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Growl",
      service_url: Some("http://growl.info"),
      setup_url: None,
      protocols: vec!["growl"],
      description: "Send notifications via Growl (GNTP).",
      attachment_support: false,
    }
  }

  fn gntp_auth_header(&self) -> String {
    match &self.password {
      None => "NONE".to_string(),
      Some(pw) => {
        // GNTP SHA256 key hashing:
        // 1. salt = 16 random bytes
        // 2. key = SHA256(password_utf8 + salt)
        // 3. key_hash = SHA256(key)
        // 4. Header: "SHA256:<hex(key_hash)>.<hex(salt)>"
        let salt: Vec<u8> = (0..16).map(|_| rand::random::<u8>()).collect();
        let mut hasher = Sha256::new();
        hasher.update(pw.as_bytes());
        hasher.update(&salt);
        let key = hasher.finalize();
        let key_hash = hex::encode(Sha256::digest(key));
        format!("SHA256:{}.{}", key_hash, hex::encode(&salt))
      }
    }
  }
}

#[async_trait]
impl Notify for Growl {
  fn schemas(&self) -> &[&str] {
    &["growl"]
  }
  fn service_name(&self) -> &str {
    "Growl"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let auth = self.gntp_auth_header();
    let mut stream =
      TcpStream::connect(format!("{}:{}", self.host, self.port)).await.map_err(|e| NotifyError::Other(format!("Growl connect failed: {}", e)))?;

    // REGISTER
    let register = format!(
      "GNTP/1.0 REGISTER NONE {}
Application-Name: Apprise
Notifications-Count: 1

Notification-Name: Alert
Notification-Display-Name: Alert
Notification-Enabled: Yes

",
      auth
    );
    stream.write_all(register.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;

    // Read REGISTER response
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await.map_err(|e| NotifyError::Other(e.to_string()))?;
    let resp = String::from_utf8_lossy(&buf[..n]);
    if !resp.contains("GNTP/1.0 -OK") && !resp.starts_with("GNTP/1.0 -OK") {
      return Err(NotifyError::Other(format!("Growl REGISTER failed: {}", resp.trim())));
    }

    // NOTIFY
    let notify = format!(
      "GNTP/1.0 NOTIFY NONE {}
Application-Name: Apprise
Notification-Name: Alert
Notification-Title: {}
Notification-Text: {}

",
      auth,
      ctx.title.replace("\r\n", " "),
      ctx.body.replace("\r\n", " "),
    );
    stream.write_all(notify.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;

    let n = stream.read(&mut buf).await.map_err(|e| NotifyError::Other(e.to_string()))?;
    let resp = String::from_utf8_lossy(&buf[..n]);
    Ok(resp.contains("-OK"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::utils::parse::ParsedUrl;

  fn parse_growl(url: &str) -> Option<Growl> {
    ParsedUrl::parse(url).and_then(|p| Growl::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    let valid_urls = vec![
      "growl://pass@growl.server",
      "growl://ignored:pass@growl.server",
      "growl://growl.server",
      "growl://growl.server?version=1",
      "growl://growl.server?sticky=yes",
      "growl://growl.server?sticky=no",
      "growl://growl.server?version=2",
      "growl://pass@growl.server?priority=low",
      "growl://pass@growl.server?priority=moderate",
      "growl://pass@growl.server?priority=normal",
      "growl://pass@growl.server?priority=high",
      "growl://pass@growl.server?priority=emergency",
      "growl://pass@growl.server?priority=invalid",
      "growl://pass@growl.server?priority=",
      "growl://growl.server?version=",
      "growl://growl.server?version=crap",
      "growl://growl.changeport:2000",
      "growl://growl.garbageport:garbage",
      "growl://growl.colon:",
      "growl://localhost",
      "growl://192.168.1.1",
      "growl://user:pass@localhost",
      "growl://localhost:23053",
    ];
    for url in &valid_urls {
      assert!(parse_growl(url).is_some(), "Growl::from_url returned None for valid URL: {}", url,);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let invalid_urls = vec!["growl://", "growl://:@/"];
    for url in &invalid_urls {
      assert!(parse_growl(url).is_none(), "Growl::from_url should return None for: {}", url,);
    }
  }

  #[test]
  fn test_default_port() {
    let g = parse_growl("growl://growl.server").unwrap();
    assert_eq!(g.host, "growl.server");
    assert_eq!(g.port, 23053);
    assert!(g.password.is_none());
  }

  #[test]
  fn test_custom_port() {
    let g = parse_growl("growl://growl.changeport:2000").unwrap();
    assert_eq!(g.host, "growl.changeport");
    assert_eq!(g.port, 2000);
  }

  #[test]
  fn test_password_from_url() {
    // growl://user:pass@host -- password is in the password field
    let g = parse_growl("growl://ignored:pass@growl.server").unwrap();
    assert_eq!(g.password.as_deref(), Some("pass"));
  }

  #[test]
  fn test_user_only_no_password() {
    // growl://pass@host -- "pass" is the user field, password is None
    let g = parse_growl("growl://pass@growl.server").unwrap();
    assert!(g.password.is_none());
  }

  #[test]
  fn test_no_password() {
    let g = parse_growl("growl://growl.server").unwrap();
    assert!(g.password.is_none());
  }

  #[test]
  fn test_service_details() {
    let details = Growl::static_details();
    assert_eq!(details.service_name, "Growl");
    assert_eq!(details.protocols, vec!["growl"]);
    assert!(!details.attachment_support);
  }

  #[test]
  fn test_gntp_auth_header_no_password() {
    let g = parse_growl("growl://growl.server").unwrap();
    assert_eq!(g.gntp_auth_header(), "NONE");
  }

  #[test]
  fn test_gntp_auth_header_with_password() {
    // Use user:pass form so password is set
    let g = parse_growl("growl://user:mypass@growl.server").unwrap();
    let header = g.gntp_auth_header();
    assert!(header.starts_with("SHA256:"), "Expected SHA256 auth header, got: {}", header);
    // Should contain hex(key_hash).hex(salt)
    assert!(header.contains('.'), "Auth header should have dot separator");
  }
}
