use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Workflows { workflow_url: String, verify_certificate: bool, tags: Vec<String> }
impl Workflows {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let path = if url.path.is_empty() { String::new() } else { format!("/{}", url.path) };
        let workflow_url = format!("https://{}{}", host, path);
        Some(Self { workflow_url, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Microsoft Workflows", service_url: Some("https://make.powerautomate.com"), setup_url: None, protocols: vec!["workflows"], description: "Send via Microsoft Power Automate Workflows.", attachment_support: false } }
}
#[async_trait]
impl Notify for Workflows {
    fn schemas(&self) -> &[&str] { &["workflows"] }
    fn service_name(&self) -> &str { "Microsoft Workflows" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "title": ctx.title, "text": ctx.body, "type": ctx.notify_type.to_string() });
        let resp = client.post(&self.workflow_url).header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
