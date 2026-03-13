use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Fluxer { webhook_id: String, token: String, verify_certificate: bool, tags: Vec<String> }
impl Fluxer {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let webhook_id = url.host.clone()?;
        let token = url.path_parts.first()?.clone();
        Some(Self { webhook_id, token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Fluxer", service_url: None, setup_url: None, protocols: vec!["fluxer"], description: "Send via Fluxer webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Fluxer {
    fn schemas(&self) -> &[&str] { &["fluxer"] }
    fn service_name(&self) -> &str { "Fluxer" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.fluxer.io/webhooks/{}/{}", self.webhook_id, self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "content": text });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
