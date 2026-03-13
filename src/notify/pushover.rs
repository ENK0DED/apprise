use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct Pushover {
    user_key: String,
    token: String,
    priority: i32,
    sound: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Pushover {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // pover://userkey@token
        let token = url.host.clone()?;
        let user_key = url.user.clone()?;
        let priority = url.get("priority").and_then(|p| p.parse().ok()).unwrap_or(0);
        let sound = url.get("sound").map(|s| s.to_string());
        Some(Self { user_key, token, priority, sound, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Pushover", service_url: Some("https://pushover.net"), setup_url: None, protocols: vec!["pover"], description: "Send notifications via Pushover.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Pushover {
    fn schemas(&self) -> &[&str] { &["pover"] }
    fn service_name(&self) -> &str { "Pushover" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut payload = json!({
            "token": self.token,
            "user": self.user_key,
            "message": ctx.body,
            "title": ctx.title,
            "priority": self.priority,
        });
        if let Some(ref sound) = self.sound { payload["sound"] = json!(sound); }
        let resp = client.post("https://api.pushover.net/1/messages.json").header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
