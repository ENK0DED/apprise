use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Notifico { project_id: String, msghook: String, verify_certificate: bool, tags: Vec<String> }
impl Notifico {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let project_id = url.host.clone()?;
        let msghook = url.path_parts.first()?.clone();
        Some(Self { project_id, msghook, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Notifico", service_url: Some("https://notico.re"), setup_url: None, protocols: vec!["notifico"], description: "Send IRC notifications via Notifico.", attachment_support: false } }
}
#[async_trait]
impl Notify for Notifico {
    fn schemas(&self) -> &[&str] { &["notifico"] }
    fn service_name(&self) -> &str { "Notifico" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let url = format!("https://notico.re/api/{}/{}", self.project_id, self.msghook);
        let client = build_client(self.verify_certificate)?;
        let resp = client.get(&url).header("User-Agent", APP_ID).query(&[("msg", msg.as_str())]).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
