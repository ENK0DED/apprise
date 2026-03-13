use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Chanify { token: String, verify_certificate: bool, tags: Vec<String> }
impl Chanify {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Chanify", service_url: Some("https://chanify.net"), setup_url: None, protocols: vec!["chanify"], description: "Send notifications via Chanify.", attachment_support: false } }
}
#[async_trait]
impl Notify for Chanify {
    fn schemas(&self) -> &[&str] { &["chanify"] }
    fn service_name(&self) -> &str { "Chanify" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.chanify.net/v1/sender/{}", self.token);
        let params = [("title", ctx.title.as_str()), ("text", ctx.body.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
