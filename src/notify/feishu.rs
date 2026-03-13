use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct FeiShu { token: String, verify_certificate: bool, tags: Vec<String> }
impl FeiShu {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "FeiShu", service_url: Some("https://open.feishu.cn"), setup_url: None, protocols: vec!["feishu"], description: "Send via FeiShu (Lark) bot webhook.", attachment_support: false } }
}
#[async_trait]
impl Notify for FeiShu {
    fn schemas(&self) -> &[&str] { &["feishu"] }
    fn service_name(&self) -> &str { "FeiShu" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://open.feishu.cn/open-apis/bot/v2/hook/{}", self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let payload = json!({ "msg_type": "text", "content": { "text": text } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
