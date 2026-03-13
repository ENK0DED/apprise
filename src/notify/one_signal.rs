use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct OneSignal { apikey: String, app_id: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl OneSignal {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let app_id = url.user.clone()?;
        let apikey = url.password.clone()?;
        let targets = url.path_parts.clone();
        Some(Self { apikey, app_id, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "OneSignal", service_url: Some("https://onesignal.com"), setup_url: None, protocols: vec!["onesignal"], description: "Send push notifications via OneSignal.", attachment_support: false } }
}
#[async_trait]
impl Notify for OneSignal {
    fn schemas(&self) -> &[&str] { &["onesignal"] }
    fn service_name(&self) -> &str { "OneSignal" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let mut payload = json!({ "app_id": self.app_id, "headings": { "en": ctx.title }, "contents": { "en": ctx.body } });
        if self.targets.is_empty() { payload["included_segments"] = json!(["All"]); } else { payload["include_player_ids"] = json!(self.targets); }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://onesignal.com/api/v1/notifications").header("User-Agent", APP_ID).header("Authorization", format!("Basic {}", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
