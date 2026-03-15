use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Smtp2Go { api_key: String, from: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Smtp2Go {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Require user@ for the from address
        let user = url.user.clone()?;
        if user.is_empty() { return None; }
        // Reject quotes in user
        if user.contains('"') { return None; }
        let api_key = url.host.clone()?;
        let from = url.get("from").unwrap_or("apprise@example.com").to_string();
        let targets: Vec<String> = url.path_parts.iter().map(|s| {
            if s.contains('@') { s.clone() } else { format!("{}@example.com", s) }
        }).collect();
        if targets.is_empty() { return None; }
        Some(Self { api_key, from, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SMTP2Go", service_url: Some("https://www.smtp2go.com"), setup_url: None, protocols: vec!["smtp2go"], description: "Send email via SMTP2Go API.", attachment_support: true } }
}
#[async_trait]
impl Notify for Smtp2Go {
    fn schemas(&self) -> &[&str] { &["smtp2go"] }
    fn service_name(&self) -> &str { "SMTP2Go" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut payload = json!({ "api_key": self.api_key, "to": self.targets, "sender": self.from, "subject": ctx.title, "text_body": ctx.body });
        if !ctx.attachments.is_empty() {
            payload["attachments"] = json!(ctx.attachments.iter().map(|att| json!({
                "filename": att.name,
                "fileblob": base64::engine::general_purpose::STANDARD.encode(&att.data),
                "mimetype": att.mime_type,
            })).collect::<Vec<_>>());
        }
        let resp = client.post("https://api.smtp2go.com/v3/email/send").header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?format=markdown",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?format=html",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?format=text",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?+X-Customer-Campaign-ID=Apprise",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?bcc=user@example.com&cc=user2@example.com",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/test@example.com",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?to=test@example.com",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/test@example.com?name=\"Frodo\"",
            "smtp2go://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/invalid",
            "smtp2go://user@example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "smtp2go://",
            "smtp2go://:@/",
            "smtp2go://user@localhost.localdomain",
            "smtp2go://localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc",
            "smtp2go://\"@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
