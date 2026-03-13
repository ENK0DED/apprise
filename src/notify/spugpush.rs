use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct SpugPush { token: String, verify_certificate: bool, tags: Vec<String> }
impl SpugPush {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SpugPush", service_url: Some("https://spug.cc"), setup_url: None, protocols: vec!["spugpush"], description: "Send push via Spug.", attachment_support: false } }
}
#[async_trait]
impl Notify for SpugPush {
    fn schemas(&self) -> &[&str] { &["spugpush"] }
    fn service_name(&self) -> &str { "SpugPush" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "title": ctx.title, "content": ctx.body });
        let url = format!("https://push.spug.cc/send/{}", self.token);
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
