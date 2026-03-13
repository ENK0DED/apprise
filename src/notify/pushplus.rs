use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PushPlus { token: String, verify_certificate: bool, tags: Vec<String> }
impl PushPlus {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> { Some(Self { token: url.host.clone()?, verify_certificate: url.verify_certificate(), tags: url.tags() }) }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "PushPlus", service_url: Some("https://www.pushplus.plus"), setup_url: None, protocols: vec!["pushplus"], description: "Send notifications via PushPlus.", attachment_support: false } }
}
#[async_trait]
impl Notify for PushPlus {
    fn schemas(&self) -> &[&str] { &["pushplus"] }
    fn service_name(&self) -> &str { "PushPlus" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let payload = json!({ "token": self.token, "title": ctx.title, "content": ctx.body });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://www.pushplus.plus/send").header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
