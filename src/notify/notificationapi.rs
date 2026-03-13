use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct NotificationApi { client_id: String, secret: String, verify_certificate: bool, tags: Vec<String> }
impl NotificationApi {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let client_id = url.user.clone()?;
        let secret = url.password.clone()?;
        Some(Self { client_id, secret, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "NotificationAPI", service_url: Some("https://www.notificationapi.com"), setup_url: None, protocols: vec!["napi", "notificationapi"], description: "Send via NotificationAPI.", attachment_support: false } }
}
#[async_trait]
impl Notify for NotificationApi {
    fn schemas(&self) -> &[&str] { &["napi", "notificationapi"] }
    fn service_name(&self) -> &str { "NotificationAPI" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let payload = json!({ "notificationId": "apprise", "user": { "id": "default" }, "mergeTags": { "title": ctx.title, "body": ctx.body } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.notificationapi.com/send").header("User-Agent", APP_ID).basic_auth(&self.client_id, Some(&self.secret)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
