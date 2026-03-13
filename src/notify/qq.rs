use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Qq { token: String, verify_certificate: bool, tags: Vec<String> }
impl Qq {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> { Some(Self { token: url.host.clone()?, verify_certificate: url.verify_certificate(), tags: url.tags() }) }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "QQ (Qmsg)", service_url: Some("https://qmsg.zendee.cn"), setup_url: None, protocols: vec!["qq"], description: "Send notifications via QQ Qmsg.", attachment_support: false } }
}
#[async_trait]
impl Notify for Qq {
    fn schemas(&self) -> &[&str] { &["qq"] }
    fn service_name(&self) -> &str { "QQ (Qmsg)" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let url = format!("https://qmsg.zendee.cn/send/{}", self.token);
        let params = [("msg", msg.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
