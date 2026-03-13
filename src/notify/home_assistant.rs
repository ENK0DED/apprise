use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct HomeAssistant {
    host: String,
    port: Option<u16>,
    access_token: String,
    secure: bool,
    notification_id: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl HomeAssistant {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // hassio://access_token@host[:port]
        let host = url.host.clone()?;
        let access_token = url.user.clone().or_else(|| url.password.clone())?;
        let notification_id = url.get("id").map(|s| s.to_string());
        Some(Self { host, port: url.port, access_token, secure: url.schema == "hassios", notification_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Home Assistant", service_url: Some("https://www.home-assistant.io"), setup_url: None, protocols: vec!["hassio", "hassios"], description: "Send via Home Assistant persistent notifications.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for HomeAssistant {
    fn schemas(&self) -> &[&str] { &["hassio", "hassios"] }
    fn service_name(&self) -> &str { "Home Assistant" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/api/services/persistent_notification/create", schema, self.host, port_str);
        let mut payload = json!({ "title": ctx.title, "message": ctx.body });
        if let Some(ref id) = self.notification_id { payload["notification_id"] = json!(id); }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.access_token)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
