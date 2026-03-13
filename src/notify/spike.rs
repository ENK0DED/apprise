use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Spike { channel_key: String, verify_certificate: bool, tags: Vec<String> }
impl Spike {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let channel_key = url.host.clone()?;
        Some(Self { channel_key, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Spike", service_url: Some("https://spike.sh"), setup_url: None, protocols: vec!["spike"], description: "Send alerts via Spike.sh.", attachment_support: false } }
}
#[async_trait]
impl Notify for Spike {
    fn schemas(&self) -> &[&str] { &["spike"] }
    fn service_name(&self) -> &str { "Spike" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "title": ctx.title, "message": ctx.body, "status": ctx.notify_type.to_string() });
        let url = format!("https://api.spike.sh/api/v1/integration/webhook/{}", self.channel_key);
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
