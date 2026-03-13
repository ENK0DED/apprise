use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Ifttt { webhook_id: String, events: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Ifttt {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let webhook_id = url.host.clone()?;
        let events = url.path_parts.clone();
        if events.is_empty() { return None; }
        Some(Self { webhook_id, events, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "IFTTT", service_url: Some("https://ifttt.com"), setup_url: None, protocols: vec!["ifttt"], description: "Trigger IFTTT webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Ifttt {
    fn schemas(&self) -> &[&str] { &["ifttt"] }
    fn service_name(&self) -> &str { "IFTTT" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for event in &self.events {
            let url = format!("https://maker.ifttt.com/trigger/{}/with/key/{}", event, self.webhook_id);
            let payload = json!({ "value1": ctx.title, "value2": ctx.body, "value3": ctx.notify_type.as_str() });
            let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
