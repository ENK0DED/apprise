use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct WeComBot { key: String, verify_certificate: bool, tags: Vec<String> }
impl WeComBot {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let key = url.host.clone()?;
        Some(Self { key, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "WeCom Bot", service_url: Some("https://work.weixin.qq.com"), setup_url: None, protocols: vec!["wecombot"], description: "Send messages via WeCom group robot.", attachment_support: false } }
}
#[async_trait]
impl Notify for WeComBot {
    fn schemas(&self) -> &[&str] { &["wecombot"] }
    fn service_name(&self) -> &str { "WeCom Bot" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}\n", ctx.title) }, ctx.body);
        let payload = json!({ "msgtype": "text", "text": { "content": msg } });
        let url = format!("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key={}", self.key);
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
