use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Notifiarr { apikey: String, channels: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Notifiarr {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let channels = url.path_parts.clone();
        Some(Self { apikey, channels, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Notifiarr", service_url: Some("https://notifiarr.com"), setup_url: None, protocols: vec!["notifiarr"], description: "Send notifications via Notifiarr.", attachment_support: false } }
}
#[async_trait]
impl Notify for Notifiarr {
    fn schemas(&self) -> &[&str] { &["notifiarr"] }
    fn service_name(&self) -> &str { "Notifiarr" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let channel = self.channels.first().map(|s| s.as_str()).unwrap_or("0");
        let payload = json!({ "notification": { "update": false, "name": "Apprise", "event": ctx.title }, "discord": { "color": ctx.notify_type.color(), "ping": { "pingUser": 0, "pingRole": 0 }, "text": { "title": ctx.title, "content": ctx.body, "footer": "Apprise" }, "ids": { "channel": channel } } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://notifiarr.com/api/v1/notification/apprise").header("User-Agent", APP_ID).header("x-api-key", self.apikey.as_str()).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
