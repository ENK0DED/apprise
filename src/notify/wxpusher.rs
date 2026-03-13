use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct WxPusher { token: String, uids: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl WxPusher {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        let uids = url.path_parts.clone();
        if uids.is_empty() { return None; }
        Some(Self { token, uids, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "WxPusher", service_url: Some("https://wxpusher.zjiecode.com"), setup_url: None, protocols: vec!["wxpusher"], description: "Send messages via WxPusher WeChat service.", attachment_support: false } }
}
#[async_trait]
impl Notify for WxPusher {
    fn schemas(&self) -> &[&str] { &["wxpusher"] }
    fn service_name(&self) -> &str { "WxPusher" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "appToken": self.token, "content": format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}\n", ctx.title) }, ctx.body), "contentType": 1, "uids": self.uids });
        let resp = client.post("https://wxpusher.zjiecode.com/api/send/message").header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
