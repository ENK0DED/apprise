use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Signl4 { secret: String, verify_certificate: bool, tags: Vec<String> }
impl Signl4 {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> { Some(Self { secret: url.host.clone()?, verify_certificate: url.verify_certificate(), tags: url.tags() }) }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SIGNL4", service_url: Some("https://www.signl4.com"), setup_url: None, protocols: vec!["signl4"], description: "Send mobile alerts via SIGNL4.", attachment_support: false } }
}
#[async_trait]
impl Notify for Signl4 {
    fn schemas(&self) -> &[&str] { &["signl4"] }
    fn service_name(&self) -> &str { "SIGNL4" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://connect.signl4.com/webhook/{}/", self.secret);
        let payload = json!({ "Title": ctx.title, "Description": ctx.body });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
