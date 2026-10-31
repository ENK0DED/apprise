//! Delta Chat (upstream `apprise/plugins/deltachat.py`).
//!
//! Delta Chat is chat over email: this is the Email plugin with the chat
//! protocol's headers (`Chat-Version: 1.0`) and its `Chat:` subject, always
//! plain text, with the title merged into the message.
//!
//! `deltachat://user:password@smtp.example.com/friend@example.org`
//!
//! The recipient must accept the bot's address as a contact; the first
//! message lands in their "Contact Requests" until then. Many chatmail
//! relays reject unencrypted mail, so `?pgp=sign` or `?pgp=encrypt` (see the
//! Email plugin) is usually wanted.

use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::email::{Email, PgpMode, Protocol};
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::types::NotifyFormat;
use crate::utils::email::is_space;
use crate::utils::parse::ParsedUrl;

/// Longest body excerpt placed in the outer Subject header.
const SUBJECT_EXCERPT_LEN: usize = 50;

pub struct DeltaChat {
  email: Email,
  secure: bool,
}

impl DeltaChat {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let secure = url.schema == "deltachats";
    Some(Self { email: Email::parse(url, secure)?, secure })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Delta Chat",
      service_url: Some("https://delta.chat/"),
      setup_url: Some("https://appriseit.com/services/deltachat/"),
      protocols: vec!["deltachat", "deltachats"],
      description: "Send Delta Chat messages through an email account.",
      attachment_support: true,
    }
  }

  /// The `Chat:` subject, without exposing protected content: encrypted
  /// mail carries Delta Chat's placeholder, signed mail an empty excerpt.
  fn chat_subject(&self, body: &str) -> String {
    match self.email.pgp_mode() {
      PgpMode::Encrypt => "Chat: Encrypted message".to_string(),
      PgpMode::Sign => "Chat:".to_string(),
      PgpMode::None => {
        let excerpt = body.split(is_space).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
        let excerpt =
          if excerpt.chars().count() > SUBJECT_EXCERPT_LEN { format!("{}...", excerpt.chars().take(SUBJECT_EXCERPT_LEN).collect::<String>()) } else { excerpt };
        if excerpt.is_empty() { "Chat:".to_string() } else { format!("Chat: {}", excerpt) }
      }
    }
  }
}

#[async_trait]
impl Notify for DeltaChat {
  fn schemas(&self) -> &[&str] {
    &["deltachat", "deltachats"]
  }
  fn service_name(&self) -> &str {
    "Delta Chat"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.email.tags()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  // Delta Chat shows plain text; upstream forces format=text
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Text
  }
  fn notify_format_overridable(&self) -> bool {
    false
  }
  // There is no title in a chat bubble: the title goes into the body
  fn title_maxlen(&self) -> usize {
    0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(self.email.identifier(if self.secure { "deltachats" } else { "deltachat" }))
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let protocol = Protocol { subject: Some(self.chat_subject(&ctx.body)), headers: &[("Chat-Version", "1.0")] };
    self.email.deliver(ctx, protocol).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::email::tests::{FakeSmtp, ServerConfig, against, ctx, notify_url};

  fn parse(url: &str) -> Option<DeltaChat> {
    DeltaChat::from_url(&ParsedUrl::parse(url)?)
  }

  #[test]
  fn test_urls() {
    // Valid and invalid URLs from upstream test_plugin_deltachat.py
    assert!(parse("deltachat://").is_none());
    for url in [
      "deltachat://user:pass@smtp.example.com/friend@example.org",
      "deltachats://user:pass@smtp.example.com/friend@example.org",
      "deltachats://user:pass@smtp.example.com:2525/friend@example.org",
      "deltachat://user:pass@smtp.example.com/",
      "deltachat://user:pass@smtp.example.com/friend@example.org?pgp=sign",
    ] {
      assert!(parse(url).is_some(), "{}", url);
    }
    let d = parse("deltachat://user:pass@smtp.example.com/friend@example.org").unwrap();
    assert_eq!(d.schemas(), &["deltachat", "deltachats"]);
    assert_eq!(d.notify_formats(), vec![NotifyFormat::Text]);
    assert!(!d.notify_format_overridable());
    assert_eq!(d.title_maxlen(), 0);
    assert_eq!(d.url_identifier().unwrap()[0], "deltachat");
    assert_eq!(parse("deltachats://user:pass@smtp.example.com/a@example.org").unwrap().url_identifier().unwrap()[0], "deltachats");
  }

  #[test]
  fn test_chat_subject() {
    let d = parse("deltachat://user:pass@smtp.example.com/friend@example.org").unwrap();
    assert_eq!(d.chat_subject("hello"), "Chat: hello");
    assert_eq!(d.chat_subject("  multi\r\nline   body "), "Chat: multi line body");
    assert_eq!(d.chat_subject(""), "Chat:");
    assert_eq!(d.chat_subject(" \n "), "Chat:");
    let long = "é".repeat(60);
    assert_eq!(d.chat_subject(&long), format!("Chat: {}...", "é".repeat(50)));
    assert_eq!(d.chat_subject(&"x".repeat(50)), format!("Chat: {}", "x".repeat(50)));

    let signed = parse("deltachat://user:pass@smtp.example.com/friend@example.org?pgp=sign").unwrap();
    assert_eq!(signed.chat_subject("secret"), "Chat:");
    let encrypted = parse("deltachat://user:pass@smtp.example.com/friend@example.org?pgp=encrypt").unwrap();
    assert_eq!(encrypted.chat_subject("secret"), "Chat: Encrypted message");
  }

  /// Upstream test_plugin_deltachat_send_envelope and _forces_text_format.
  #[tokio::test]
  async fn test_send_envelope() {
    let server = FakeSmtp::start(ServerConfig::default()).await;
    let url = "deltachat://user:pass@smtp.example.com/friend@example.org?format=html";
    let (ok, requested) = against(&server, notify_url(url, &ctx("My Title", "hello world", NotifyFormat::Html))).await;
    assert!(ok);
    assert_eq!(requested, vec![("smtp.example.com".to_string(), 25)]);
    let text = &server.mails()[0].text;
    assert!(text.contains("\nChat-Version: 1.0\n"), "{}", text);
    assert!(text.contains("\nSubject: Chat: My Title hello world\n"), "{}", text);
    assert!(text.starts_with("Content-Type: text/plain; charset=\"utf-8\"\n"), "{}", text);
    assert!(!text.contains("text/html"));
  }

  /// Upstream test_plugin_deltachat_autocrypt_and_pgp_inherited: signed,
  /// with the generated key advertised once and no body in the subject.
  #[cfg(feature = "pgp")]
  #[tokio::test]
  async fn test_signed_with_autocrypt() {
    let store = tempfile::tempdir().unwrap();
    let server = FakeSmtp::start(ServerConfig::default()).await;
    // The sender's pair, as obj.pgp.keygen() does upstream
    let generated = store.path().join("keys");
    let pgp = crate::utils::pgp::PgpController::new(None, None, Some("user@smtp.example.com"), "Apprise", None).with_path(Some(generated.clone()));
    assert!(pgp.keygen(None, None, false).await);
    let prv = generated.join("user-prv.asc");
    let mut apprise = crate::Apprise::with_asset(crate::asset::AppriseAsset { storage_path: Some(store.path().to_path_buf()), ..Default::default() });
    assert!(apprise.add(&format!("deltachat://user:pass@smtp.example.com/friend@example.org?pgp=sign&pgpprv={}", prv.display())));
    let (result, _) = against(&server, apprise.notify(&ctx("T", "secret chat message", NotifyFormat::Text))).await;
    assert!(result.is_success());
    let text = &server.mails()[0].text;
    assert!(text.starts_with("Content-Type: multipart/signed;"), "{}", text);
    assert!(text.contains("\nChat-Version: 1.0\n"));
    assert_eq!(text.matches("Autocrypt:").count(), 1);
    assert!(text.contains("\nSubject: Chat:\n"));
    assert!(!text.contains("Subject: Chat: T secret"));
  }

  /// Upstream test_plugin_deltachat_encrypt_subject_privacy.
  #[cfg(feature = "pgp")]
  #[tokio::test]
  async fn test_encrypted_subject_privacy() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("pub.asc");
    std::fs::write(&key, crate::notify::email::tests::PGPY_PUB).unwrap();
    let server = FakeSmtp::start(ServerConfig::default()).await;
    let url = format!("deltachat://user:pass@smtp.example.com/friend@example.org?pgp=encrypt&pgppub={}", key.display());
    let secret = "CONFIDENTIAL body content must remain private";
    let (ok, _) = against(&server, notify_url(&url, &ctx("T", secret, NotifyFormat::Text))).await;
    assert!(ok);
    let text = &server.mails()[0].text;
    assert!(text.contains("\nSubject: Chat: Encrypted message\n"), "{}", text);
    assert!(!text.contains("CONFIDENTIAL"));
  }
}
