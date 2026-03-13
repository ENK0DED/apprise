use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PushMe { token: String, verify_certificate: bool, tags: Vec<String> }
impl PushMe {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> { Some(Self { token: url.host.clone()?, verify_certificate: url.verify_certificate(), tags: url.tags() }) }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "PushMe", service_url: Some("https://push.i-i.me"), setup_url: None, protocols: vec!["pushme"], description: "Send notifications via PushMe.", attachment_support: false } }
}
#[async_trait]
impl Notify for PushMe {
    fn schemas(&self) -> &[&str] { &["pushme"] }
    fn service_name(&self) -> &str { "PushMe" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let params = [("push_key", self.token.as_str()), ("title", ctx.title.as_str()), ("content", ctx.body.as_str()), ("type", "markdown")];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://push.i-i.me/").header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
