use async_trait::async_trait;
use lettre::{
  AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
  message::{Mailbox, MultiPart, SinglePart, header::ContentType},
  transport::smtp::authentication::Credentials,
};

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

#[allow(dead_code)]
pub struct Email {
  smtp_host: String,
  smtp_port: u16,
  secure: SecureMode,
  from: String,
  from_name: Option<String>,
  to: Vec<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  reply_to: Vec<String>,
  user: Option<String>,
  password: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum SecureMode {
  Plain,
  Ssl,
  StartTls,
}

/// Known email provider SMTP templates (matching Python's EMAIL_TEMPLATES)
struct SmtpDefaults {
  smtp_host: &'static str,
  port: u16,
  secure: SecureMode,
}

fn detect_smtp_defaults(domain: &str) -> Option<SmtpDefaults> {
  let d = domain.to_lowercase();
  match d.as_str() {
    "gmail.com" | "googlemail.com" => Some(SmtpDefaults { smtp_host: "smtp.gmail.com", port: 587, secure: SecureMode::StartTls }),
    "yahoo.com" | "yahoo.ca" | "yahoo.co.uk" | "yahoo.co.jp" | "ymail.com" | "rocketmail.com" => {
      Some(SmtpDefaults { smtp_host: "smtp.mail.yahoo.com", port: 465, secure: SecureMode::Ssl })
    }
    "hotmail.com" | "live.com" | "outlook.com" | "msn.com" => {
      Some(SmtpDefaults { smtp_host: "smtp-mail.outlook.com", port: 587, secure: SecureMode::StartTls })
    }
    "fastmail.com" | "fastmail.fm" => Some(SmtpDefaults { smtp_host: "smtp.fastmail.com", port: 465, secure: SecureMode::Ssl }),
    "protonmail.com" | "proton.me" | "pm.me" => Some(SmtpDefaults { smtp_host: "smtp.protonmail.ch", port: 587, secure: SecureMode::StartTls }),
    "zoho.com" | "zohomail.com" => Some(SmtpDefaults { smtp_host: "smtp.zoho.com", port: 465, secure: SecureMode::Ssl }),
    "aol.com" => Some(SmtpDefaults { smtp_host: "smtp.aol.com", port: 465, secure: SecureMode::Ssl }),
    "icloud.com" | "mac.com" | "me.com" => Some(SmtpDefaults { smtp_host: "smtp.mail.me.com", port: 587, secure: SecureMode::StartTls }),
    "mail.com" | "email.com" => Some(SmtpDefaults { smtp_host: "smtp.mail.com", port: 587, secure: SecureMode::StartTls }),
    "gmx.com" | "gmx.de" | "gmx.net" => Some(SmtpDefaults { smtp_host: "mail.gmx.com", port: 465, secure: SecureMode::Ssl }),
    "163.com" => Some(SmtpDefaults { smtp_host: "smtp.163.com", port: 465, secure: SecureMode::Ssl }),
    "qq.com" => Some(SmtpDefaults { smtp_host: "smtp.qq.com", port: 465, secure: SecureMode::Ssl }),
    "sendgrid.net" => Some(SmtpDefaults { smtp_host: "smtp.sendgrid.net", port: 587, secure: SecureMode::StartTls }),
    _ => None,
  }
}

impl Email {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;

    let user = url.user.clone();
    let password = url.password.clone();

    // Determine from address
    let from = url
      .get("from")
      .map(|s| s.to_string())
      .or_else(|| user.as_ref().map(|u| if u.contains('@') { u.clone() } else { format!("{}@{}", u, host) }))
      .unwrap_or_else(|| format!("noreply@{}", host));

    let from_name = url.get("name").map(|s| s.to_string());

    // Try auto-detecting SMTP settings from email domain
    let from_domain = from.rsplit('@').next().unwrap_or("");
    let defaults = detect_smtp_defaults(from_domain);

    // Override SMTP host if ?smtp= is set, otherwise try auto-detect, fallback to host
    let smtp_host = url.get("smtp").map(|s| s.to_string()).or_else(|| defaults.as_ref().map(|d| d.smtp_host.to_string())).unwrap_or_else(|| host.clone());

    let secure = match url.schema.as_str() {
      "mailtos" => {
        let mode = url.get("mode").unwrap_or("");
        match mode {
          "ssl" => SecureMode::Ssl,
          "insecure" | "plain" => SecureMode::Plain,
          _ => defaults.as_ref().map(|d| d.secure.clone()).unwrap_or(SecureMode::StartTls),
        }
      }
      _ => {
        let mode = url.get("mode").unwrap_or("");
        match mode {
          "ssl" => SecureMode::Ssl,
          "starttls" => SecureMode::StartTls,
          _ => defaults.as_ref().map(|d| d.secure.clone()).unwrap_or(SecureMode::Plain),
        }
      }
    };

    let default_port = match &secure {
      SecureMode::Ssl => 465,
      SecureMode::StartTls => 587,
      SecureMode::Plain => 25,
    };
    let smtp_port = url.port.or_else(|| defaults.as_ref().map(|d| d.port)).unwrap_or(default_port);

    // Collect targets from path + "to" param
    let mut to: Vec<String> = url.path_parts.clone();
    if let Some(t) = url.get("to") {
      to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    if to.is_empty() {
      to.push(from.clone());
    }

    // CC recipients
    let cc: Vec<String> = url.get("cc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();

    // BCC recipients
    let bcc: Vec<String> = url.get("bcc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();

    // Reply-To addresses
    let reply_to: Vec<String> = url.get("reply").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();

    Some(Self {
      smtp_host,
      smtp_port,
      secure,
      from,
      from_name,
      to,
      cc,
      bcc,
      reply_to,
      user,
      password,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Email (SMTP)",
      service_url: None,
      setup_url: Some("https://github.com/caronc/apprise/wiki/Notify_email"),
      protocols: vec!["mailto", "mailtos"],
      description: "Send notifications via SMTP email.",
      attachment_support: true,
    }
  }
}

#[async_trait]
impl Notify for Email {
  fn schemas(&self) -> &[&str] {
    &["mailto", "mailtos"]
  }
  fn service_name(&self) -> &str {
    "Email (SMTP)"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let from_mailbox: Mailbox = if let Some(ref name) = self.from_name {
      format!("{} <{}>", name, self.from).parse().map_err(|e| NotifyError::Email(format!("Invalid from address: {}", e)))?
    } else {
      self.from.parse().map_err(|e| NotifyError::Email(format!("Invalid from address: {}", e)))?
    };

    let subject = if ctx.title.is_empty() { "Apprise Notification".to_string() } else { ctx.title.clone() };

    let mut all_ok = true;

    for to_addr in &self.to {
      let to_mailbox: Mailbox = to_addr.parse().map_err(|e| NotifyError::Email(format!("Invalid to address {}: {}", to_addr, e)))?;

      let mut builder = Message::builder().from(from_mailbox.clone()).to(to_mailbox).subject(&subject);

      // Add CC recipients
      for cc_addr in &self.cc {
        if let Ok(mb) = cc_addr.parse::<Mailbox>() {
          builder = builder.cc(mb);
        }
      }

      // Add BCC recipients
      for bcc_addr in &self.bcc {
        if let Ok(mb) = bcc_addr.parse::<Mailbox>() {
          builder = builder.bcc(mb);
        }
      }

      // Add Reply-To addresses
      for reply_addr in &self.reply_to {
        if let Ok(mb) = reply_addr.parse::<Mailbox>() {
          builder = builder.reply_to(mb);
        }
      }

      let email = if ctx.attachments.is_empty() {
        builder.body(ctx.body.clone()).map_err(|e| NotifyError::Email(e.to_string()))?
      } else {
        // Build multipart message with attachments
        let text_part = SinglePart::builder().header(ContentType::TEXT_PLAIN).body(ctx.body.clone());
        let mut mp = MultiPart::mixed().singlepart(text_part);
        for att in &ctx.attachments {
          let ct = att.mime_type.parse::<ContentType>().unwrap_or(ContentType::TEXT_PLAIN);
          let att_part = SinglePart::builder().header(ct).header(lettre::message::header::ContentDisposition::attachment(&att.name)).body(att.data.clone());
          mp = mp.singlepart(att_part);
        }
        builder.multipart(mp).map_err(|e| NotifyError::Email(e.to_string()))?
      };

      let result = match &self.secure {
        SecureMode::Ssl => {
          let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(&self.smtp_host)
            .map_err(|e| NotifyError::Email(e.to_string()))?
            .port(self.smtp_port)
            .credentials(self.make_creds())
            .build();
          transport.send(email).await
        }
        SecureMode::StartTls => {
          let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.smtp_host)
            .map_err(|e| NotifyError::Email(e.to_string()))?
            .port(self.smtp_port)
            .credentials(self.make_creds())
            .build();
          transport.send(email).await
        }
        SecureMode::Plain => {
          let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.smtp_host).port(self.smtp_port).credentials(self.make_creds()).build();
          transport.send(email).await
        }
      };

      match result {
        Ok(_) => tracing::info!("Email sent to {}", to_addr),
        Err(e) => {
          tracing::warn!("Email to {} failed: {}", to_addr, e);
          all_ok = false;
        }
      }
    }
    Ok(all_ok)
  }
}

impl Email {
  fn make_creds(&self) -> Credentials {
    match (&self.user, &self.password) {
      (Some(u), Some(p)) => Credentials::new(u.clone(), p.clone()),
      (Some(u), None) => Credentials::new(u.clone(), String::new()),
      _ => Credentials::new(String::new(), String::new()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::utils::parse::ParsedUrl;

  #[test]
  fn test_valid_urls() {
    let valid_urls = vec!["mailto://user:pass@gmail.com", "mailtos://user:pass@gmail.com", "mailto://user:pass@gmail.com/recipient@example.com"];
    for url in &valid_urls {
      let parsed = ParsedUrl::parse(url);
      assert!(parsed.is_some(), "ParsedUrl::parse failed for: {}", url);
      let parsed = parsed.unwrap();
      assert!(Email::from_url(&parsed).is_some(), "Email::from_url returned None for valid URL: {}", url,);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let invalid_urls = vec!["mailto://", "mailto://:@/"];
    for url in &invalid_urls {
      let result = ParsedUrl::parse(url).and_then(|p| Email::from_url(&p));
      assert!(result.is_none(), "Email::from_url should return None for: {}", url,);
    }
  }

  #[test]
  fn test_email_gmail_smtp_defaults() {
    let parsed = ParsedUrl::parse("mailto://user:pass@gmail.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.smtp_host, "smtp.gmail.com");
    assert_eq!(e.smtp_port, 587);
    assert_eq!(e.secure, SecureMode::StartTls);
    assert_eq!(e.from, "user@gmail.com");
  }

  #[test]
  fn test_email_yahoo_smtp_defaults() {
    let parsed = ParsedUrl::parse("mailto://user:pass@yahoo.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.smtp_host, "smtp.mail.yahoo.com");
    assert_eq!(e.smtp_port, 465);
    assert_eq!(e.secure, SecureMode::Ssl);
  }

  #[test]
  fn test_email_hotmail_smtp_defaults() {
    let parsed = ParsedUrl::parse("mailto://user:pass@hotmail.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.smtp_host, "smtp-mail.outlook.com");
    assert_eq!(e.smtp_port, 587);
  }

  #[test]
  fn test_email_custom_smtp_host() {
    let parsed = ParsedUrl::parse("mailto://user:pass@example.com?smtp=mail.example.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.smtp_host, "mail.example.com");
  }

  #[test]
  fn test_email_explicit_recipient() {
    let parsed = ParsedUrl::parse("mailto://user:pass@gmail.com/recipient@example.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert!(e.to.contains(&"recipient@example.com".to_string()));
  }

  #[test]
  fn test_email_default_to_self() {
    let parsed = ParsedUrl::parse("mailto://user:pass@gmail.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert!(e.to.contains(&"user@gmail.com".to_string()));
  }

  #[test]
  fn test_email_cc_bcc() {
    let parsed = ParsedUrl::parse("mailto://user:pass@gmail.com?cc=cc@example.com&bcc=bcc@example.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert!(e.cc.contains(&"cc@example.com".to_string()));
    assert!(e.bcc.contains(&"bcc@example.com".to_string()));
  }

  #[test]
  fn test_email_from_override() {
    let parsed = ParsedUrl::parse("mailto://user:pass@gmail.com?from=custom@example.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.from, "custom@example.com");
  }

  #[test]
  fn test_email_secure_modes() {
    // mailtos defaults to StartTls
    let parsed = ParsedUrl::parse("mailtos://user:pass@example.com").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert!(e.secure == SecureMode::StartTls || e.secure == SecureMode::Ssl);

    // explicit ssl mode
    let parsed = ParsedUrl::parse("mailto://user:pass@example.com?mode=ssl").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.secure, SecureMode::Ssl);

    // explicit starttls mode
    let parsed = ParsedUrl::parse("mailto://user:pass@example.com?mode=starttls").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.secure, SecureMode::StartTls);
  }

  #[test]
  fn test_email_from_name() {
    let parsed = ParsedUrl::parse("mailto://user:pass@gmail.com?name=John+Doe").unwrap();
    let e = Email::from_url(&parsed).unwrap();
    assert_eq!(e.from_name.as_deref(), Some("John Doe"));
  }

  #[test]
  fn test_email_static_details() {
    let details = Email::static_details();
    assert_eq!(details.service_name, "Email (SMTP)");
    assert_eq!(details.protocols, vec!["mailto", "mailtos"]);
    assert!(details.attachment_support);
  }
}
