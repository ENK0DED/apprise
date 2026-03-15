use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Viber { token: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Viber {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()
            .filter(|h| !h.is_empty())
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        if token.trim().is_empty() { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Viber", service_url: Some("https://www.viber.com"), setup_url: None, protocols: vec!["viber"], description: "Send messages via Viber Bot API.", attachment_support: false } }
}
#[async_trait]
impl Notify for Viber {
    fn schemas(&self) -> &[&str] { &["viber"] }
    fn service_name(&self) -> &str { "Viber" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "auth_token": self.token, "receiver": target, "type": "text", "text": msg, "sender": { "name": "Apprise" } });
            let resp = client.post("https://chatapi.viber.com/pa/send_message").header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "viber://token/contact_id",
            "viber://token/contact_id/contact2",
            "viber://token/contact_id/?type=carousel",
            "viber://token/contact_id/?type=text",
            "viber://token/contact_id/?image=http://example.com/image.jpg",
            "viber://token/?to=contact_id",
            "viber://token/?to=abc,def",
            "viber://token/m12/?from=aaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "viber://token/m12/?from=validname",
            "viber://token/contact/?type=text",
            "viber://token/contact/?type=carousel",
            "viber://tokena",
            "viber://token/contact/?format=markdown",
            "viber://token/contact/?format=html",
            "viber://token/contact/?format=text",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "viber://",
            "viber://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
