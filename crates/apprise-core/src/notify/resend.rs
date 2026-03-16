use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;

pub struct Resend {
  apikey: String,
  from_email: String,
  to: Vec<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  reply_to: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Resend {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // resend://apikey:from_name@from_domain/to@email
    // or resend://?apikey=x&from=email&to=email
    let (apikey, from_email, to) = if let Some(ref ak) = url.get("apikey") {
      let apikey = ak.to_string();
      let from_email = url.get("from").map(|s| s.to_string()).unwrap_or_default();
      let mut to: Vec<String> = url.path_parts.clone();
      if let Some(t) = url.get("to") {
        to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
      }
      (apikey, from_email, to)
    } else if url.user.is_some() && url.password.is_some() {
      let apikey = url.user.clone()?;
      let from_email = format!("{}@{}", url.password.as_ref()?, url.host.as_ref()?);
      let mut to: Vec<String> = url.path_parts.clone();
      if let Some(t) = url.get("to") {
        to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
      }
      (apikey, from_email, to)
    } else if url.user.is_some() {
      // resend://apikey@target?from=...&name=...
      let apikey = url.user.clone()?;
      let from_email = url.get("from").map(|s| s.to_string())?; // from is required
      let host = url.host.clone().unwrap_or_default();
      let decoded_host = urlencoding::decode(&host).unwrap_or_default().into_owned();
      let mut to: Vec<String> = if !decoded_host.is_empty() { vec![decoded_host] } else { vec![] };
      to.extend(url.path_parts.clone());
      if let Some(t) = url.get("to") {
        to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
      }
      (apikey, from_email, to)
    } else {
      let apikey = url.host.clone()?;
      let from_email = url.path_parts.first()?.clone();
      let mut to: Vec<String> = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
      if let Some(t) = url.get("to") {
        to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
      }
      (apikey, from_email, to)
    };
    // Validate API key — must be alphanumeric with - and _
    if !apikey.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
      return None;
    }
    let cc: Vec<String> = url.get("cc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let bcc: Vec<String> = url.get("bcc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let reply_to = url.get("reply").map(|s| s.to_string());
    Some(Self { apikey, from_email, to, cc, bcc, reply_to, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Resend",
      service_url: Some("https://resend.com"),
      setup_url: None,
      protocols: vec!["resend"],
      description: "Send email via Resend.",
      attachment_support: true,
    }
  }
}
#[async_trait]
impl Notify for Resend {
  fn schemas(&self) -> &[&str] {
    &["resend"]
  }
  fn service_name(&self) -> &str {
    "Resend"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let subject = if ctx.title.is_empty() { "Apprise Notification" } else { ctx.title.as_str() };
    let mut all_ok = true;
    for target in &self.to {
      let mut payload = json!({
          "from": self.from_email,
          "to": [target],
          "subject": subject,
          "text": ctx.body,
      });
      if !self.cc.is_empty() {
        payload["cc"] = json!(self.cc);
      }
      if !self.bcc.is_empty() {
        payload["bcc"] = json!(self.bcc);
      }
      if let Some(ref r) = self.reply_to {
        payload["reply_to"] = json!(r);
      }
      if !ctx.attachments.is_empty() {
        payload["attachments"] = json!(
          ctx
            .attachments
            .iter()
            .map(|att| json!({
                "content": base64::engine::general_purpose::STANDARD.encode(&att.data),
                "filename": att.name,
            }))
            .collect::<Vec<_>>()
        );
      }
      let resp = client
        .post("https://api.resend.com/emails")
        .header("User-Agent", APP_ID)
        .header("Authorization", format!("Bearer {}", self.apikey))
        .json(&payload)
        .send()
        .await?;
      if !resp.status().is_success() && resp.status().as_u16() != 201 {
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

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "resend://abcd:user@example.com",
      "resend://abcd:user@example.com/newuser1@example.com",
      "resend://abcd:user@example.com/newuser2@example.com?name=Jessica",
      "resend://abcd@newuser4%40example.com?name=Ralph&from=user2@example.ca",
      "resend://?apikey=abcd&from=Joe<user@example.com>&to=newuser5@example.com",
      "resend://?apikey=abcd&from=Joe<user@example.com>&reply=John<newuser6@example.com>",
      "resend://?apikey=abcd&from=Joe<user@example.com>&reply=garbage%",
      "resend://abcd:user@example.com/newuser7@example.com?bcc=l2g@nuxref.com",
      "resend://abcd:user@example.com/newuser8@example.com?cc=l2g@nuxref.com",
      "resend://abcd:user@example.com/newuser8@example.com?cc=Chris<l2g@nuxref.com>",
      "resend://abcd:user@example.com/newuser9@example.com?to=l2g@nuxref.com",
      "resend://abcd:user@example.au/newuser02@example.au",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["resend://", "resend://:@/", "resend://abcd", "resend://abcd@host", "resend://invalid-api-key+*-d:user@example.com"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_basic_fields() {
    let parsed = ParsedUrl::parse("resend://abcd:user@example.com/newuser1@example.com").unwrap();
    let r = Resend::from_url(&parsed).unwrap();
    assert_eq!(r.apikey, "abcd");
    assert_eq!(r.from_email, "user@example.com");
    assert!(r.to.contains(&"newuser1@example.com".to_string()));
  }

  #[test]
  fn test_from_url_no_target_uses_from() {
    let parsed = ParsedUrl::parse("resend://abcd:user@example.com").unwrap();
    let r = Resend::from_url(&parsed).unwrap();
    assert_eq!(r.apikey, "abcd");
    assert_eq!(r.from_email, "user@example.com");
  }

  #[test]
  fn test_from_url_apikey_param() {
    let parsed = ParsedUrl::parse("resend://?apikey=abcd&from=Joe<user@example.com>&to=newuser5@example.com").unwrap();
    let r = Resend::from_url(&parsed).unwrap();
    assert_eq!(r.apikey, "abcd");
    assert!(r.to.contains(&"newuser5@example.com".to_string()));
  }

  #[test]
  fn test_from_url_cc_bcc() {
    let parsed = ParsedUrl::parse("resend://abcd:user@example.com/target@example.com?cc=a@b.com&bcc=c@d.com").unwrap();
    let r = Resend::from_url(&parsed).unwrap();
    assert!(r.cc.contains(&"a@b.com".to_string()));
    assert!(r.bcc.contains(&"c@d.com".to_string()));
  }

  #[test]
  fn test_from_url_reply_to() {
    let parsed = ParsedUrl::parse("resend://?apikey=abcd&from=user@example.com&reply=John<reply@example.com>").unwrap();
    let r = Resend::from_url(&parsed).unwrap();
    assert!(r.reply_to.is_some());
    assert!(r.reply_to.unwrap().contains("reply@example.com"));
  }

  #[test]
  fn test_invalid_apikey_chars() {
    let parsed = ParsedUrl::parse("resend://invalid-api-key+*-d:user@example.com").unwrap();
    assert!(Resend::from_url(&parsed).is_none());
  }

  #[test]
  fn test_static_details() {
    let details = Resend::static_details();
    assert_eq!(details.service_name, "Resend");
    assert_eq!(details.service_url, Some("https://resend.com"));
    assert!(details.protocols.contains(&"resend"));
    assert!(details.attachment_support);
  }
}
