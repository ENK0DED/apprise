use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Dot { token: String, verify_certificate: bool, tags: Vec<String> }
impl Dot {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Dot", service_url: Some("https://dot.eu.org"), setup_url: None, protocols: vec!["dot"], description: "Send via Dot notification service.", attachment_support: false } }
}
#[async_trait]
impl Notify for Dot {
    fn schemas(&self) -> &[&str] { &["dot"] }
    fn service_name(&self) -> &str { "Dot" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "token": self.token, "title": ctx.title, "body": ctx.body });
        let resp = client.post("https://dot.eu.org/push").header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
