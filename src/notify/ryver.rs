use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Ryver { organization: String, token: String, verify_certificate: bool, tags: Vec<String> }
impl Ryver {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let organization = url.host.clone()?;
        let token = url.path_parts.first()?.clone();
        Some(Self { organization, token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Ryver", service_url: Some("https://ryver.com"), setup_url: None, protocols: vec!["ryver"], description: "Send via Ryver webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Ryver {
    fn schemas(&self) -> &[&str] { &["ryver"] }
    fn service_name(&self) -> &str { "Ryver" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://{}.ryver.com/application/webhook/{}", self.organization, self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "body": text });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
