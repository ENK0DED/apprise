use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Pushbullet { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Pushbullet {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let targets = url.path_parts.clone();
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Pushbullet", service_url: Some("https://pushbullet.com"), setup_url: None, protocols: vec!["pbul"], description: "Send push notifications via Pushbullet.", attachment_support: false } }
}
#[async_trait]
impl Notify for Pushbullet {
    fn schemas(&self) -> &[&str] { &["pbul"] }
    fn service_name(&self) -> &str { "Pushbullet" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut payload = json!({ "type": "note", "title": ctx.title, "body": ctx.body });
        if !self.targets.is_empty() { payload["device_iden"] = json!(self.targets[0]); }
        let resp = client.post("https://api.pushbullet.com/v2/pushes").header("User-Agent", APP_ID).header("Access-Token", self.apikey.as_str()).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
