use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Guilded { webhook_id: String, webhook_token: String, verify_certificate: bool, tags: Vec<String> }
impl Guilded {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let webhook_id = url.host.clone()?;
        let webhook_token = url.path_parts.first()?.clone();
        Some(Self { webhook_id, webhook_token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Guilded", service_url: Some("https://guilded.gg"), setup_url: None, protocols: vec!["guilded"], description: "Send via Guilded webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Guilded {
    fn schemas(&self) -> &[&str] { &["guilded"] }
    fn service_name(&self) -> &str { "Guilded" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://media.guilded.gg/webhooks/{}/{}", self.webhook_id, self.webhook_token);
        let payload = json!({ "embeds": [{ "title": ctx.title, "description": ctx.body }] });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 204 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "guilded://",
            "guilded://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
