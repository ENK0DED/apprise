use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Brevo { apikey: String, from_email: String, to: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Brevo {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // brevo://apikey:from_name@from_domain/to@email
        let (apikey, from_email, to) = if url.user.is_some() && url.password.is_some() {
            let apikey = url.user.clone()?;
            let from_email = format!("{}@{}", url.password.as_ref()?, url.host.as_ref()?);
            let mut to: Vec<String> = url.path_parts.clone();
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
        // Validate API key — must be alphanumeric
        if !apikey.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        // Validate reply-to email if provided
        if let Some(reply) = url.get("reply") {
            let decoded = urlencoding::decode(&reply).unwrap_or_default().into_owned();
            if decoded.trim().is_empty() || !decoded.contains('@') || decoded.contains('!') || decoded.contains(' ') {
                return None;
            }
        }
        Some(Self { apikey, from_email, to, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Brevo (Sendinblue)", service_url: Some("https://brevo.com"), setup_url: None, protocols: vec!["brevo"], description: "Send email via Brevo (formerly Sendinblue).", attachment_support: true } }
}
#[async_trait]
impl Notify for Brevo {
    fn schemas(&self) -> &[&str] { &["brevo"] }
    fn service_name(&self) -> &str { "Brevo (Sendinblue)" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let to_list: Vec<_> = self.to.iter().map(|e| json!({ "email": e })).collect();
        let mut payload = json!({ "sender": { "email": self.from_email }, "to": to_list, "subject": ctx.title, "textContent": ctx.body });
        if !ctx.attachments.is_empty() {
            payload["attachment"] = json!(ctx.attachments.iter().map(|att| json!({
                "content": base64::engine::general_purpose::STANDARD.encode(&att.data),
                "name": att.name,
            })).collect::<Vec<_>>());
        }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.brevo.com/v3/smtp/email").header("User-Agent", APP_ID).header("api-key", self.apikey.as_str()).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 201 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "brevo://abcd:user@example.com?format=text",
            "brevo://abcd:user@example.com/newuser@example.com?reply=user@example.ca",
            "brevo://abcd:user@example.com/bademailaddress",
            "brevo://abcd:user@example.com/newuser@example.com?bcc=l2g@nuxref.com",
            "brevo://abcd:user@example.com/newuser@example.com?cc=l2g@nuxref.com",
            "brevo://abcd:user@example.com/newuser@example.com?to=l2g@nuxref.com",
            "brevo://abcd:user@example.au/newuser@example.au",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "brevo://",
            "brevo://:@/",
            "brevo://abcd",
            "brevo://abcd@host",
            "brevo://invalid-api-key+*-d:user@example.com",
            "brevo://abcd:user@example.com/newuser@example.com?reply=%20!",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_struct_fields() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "brevo://myapikey:sender@example.com/recipient@example.com"
        ).unwrap();
        let obj = Brevo::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "myapikey");
        assert_eq!(obj.from_email, "sender@example.com");
        assert_eq!(obj.to, vec!["recipient@example.com"]);
    }

    #[test]
    fn test_from_url_no_target_uses_from() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "brevo://abcd:user@example.com?format=text"
        ).unwrap();
        let obj = Brevo::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, "abcd");
        assert_eq!(obj.from_email, "user@example.com");
        // No explicit targets - to list should be empty
        assert!(obj.to.is_empty());
    }

    #[test]
    fn test_from_url_multiple_targets() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "brevo://abcd:user@example.com/one@test.com?to=two@test.com"
        ).unwrap();
        let obj = Brevo::from_url(&parsed).unwrap();
        assert_eq!(obj.to.len(), 2);
        assert!(obj.to.contains(&"one@test.com".to_string()));
        assert!(obj.to.contains(&"two@test.com".to_string()));
    }

    #[test]
    fn test_invalid_apikey_special_chars() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "brevo://invalid-api-key+*-d:user@example.com"
        ).unwrap();
        assert!(Brevo::from_url(&parsed).is_none());
    }

    #[test]
    fn test_invalid_reply_to() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "brevo://abcd:user@example.com/newuser@example.com?reply=%20!"
        ).unwrap();
        assert!(Brevo::from_url(&parsed).is_none());
    }

    #[test]
    fn test_service_details() {
        let details = Brevo::static_details();
        assert_eq!(details.service_name, "Brevo (Sendinblue)");
        assert_eq!(details.protocols, vec!["brevo"]);
        assert!(details.attachment_support);
    }
}
