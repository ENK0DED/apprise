use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct MessageBird { api_key: String, from: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl MessageBird {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let api_key = url.password.clone()?;
        let from = url.host.clone().unwrap_or_else(|| "Apprise".to_string());
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { api_key, from, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "MessageBird", service_url: Some("https://www.messagebird.com"), setup_url: None, protocols: vec!["msgbird"], description: "Send SMS via MessageBird.", attachment_support: false } }
}
#[async_trait]
impl Notify for MessageBird {
    fn schemas(&self) -> &[&str] { &["msgbird"] }
    fn service_name(&self) -> &str { "MessageBird" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({ "originator": self.from, "recipients": self.targets, "body": msg });
        let resp = client.post("https://rest.messagebird.com/messages").header("User-Agent", APP_ID).header("Authorization", format!("AccessKey {}", self.api_key)).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "msgbird://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
