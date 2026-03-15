use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct SparkPost { api_key: String, from: String, targets: Vec<String>, host: String, verify_certificate: bool, tags: Vec<String> }
impl SparkPost {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let api_key = url.user.clone().or_else(|| url.host.clone())?;
        let from = url.get("from").unwrap_or("apprise@sparkpost.com").to_string();
        let targets: Vec<String> = url.path_parts.iter().map(|s| if s.contains('@') { s.clone() } else { format!("{}@sparkpost.com", s) }).collect();
        if targets.is_empty() { return None; }
        let host = "api.sparkpost.com".to_string();
        Some(Self { api_key, from, targets, host, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SparkPost", service_url: Some("https://www.sparkpost.com"), setup_url: None, protocols: vec!["sparkpost"], description: "Send email via SparkPost.", attachment_support: true } }
}
#[async_trait]
impl Notify for SparkPost {
    fn schemas(&self) -> &[&str] { &["sparkpost"] }
    fn service_name(&self) -> &str { "SparkPost" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let recipients: Vec<_> = self.targets.iter().map(|t| json!({ "address": { "email": t } })).collect();
        let mut content = json!({ "from": self.from, "subject": ctx.title, "text": ctx.body });
        if !ctx.attachments.is_empty() {
            content["attachments"] = json!(ctx.attachments.iter().map(|att| json!({
                "name": att.name,
                "type": att.mime_type,
                "data": base64::engine::general_purpose::STANDARD.encode(&att.data),
            })).collect::<Vec<_>>());
        }
        let payload = json!({ "recipients": recipients, "content": content });
        let url = format!("https://{}/api/v1/transmissions", self.host);
        let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", &self.api_key).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "sparkpost://",
            "sparkpost://:@/",
            "sparkpost://user@localhost.localdomain",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
