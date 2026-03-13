use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Streamlabs { access_token: String, verify_certificate: bool, tags: Vec<String> }
impl Streamlabs {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let access_token = url.host.clone()?;
        Some(Self { access_token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Streamlabs", service_url: Some("https://streamlabs.com"), setup_url: None, protocols: vec!["strmlabs"], description: "Send alerts via Streamlabs.", attachment_support: false } }
}
#[async_trait]
impl Notify for Streamlabs {
    fn schemas(&self) -> &[&str] { &["strmlabs"] }
    fn service_name(&self) -> &str { "Streamlabs" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "access_token": self.access_token, "type": "donation", "message": ctx.body, "name": ctx.title, "identifier": "apprise" });
        let resp = client.post("https://streamlabs.com/api/v1.0/alerts").header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
