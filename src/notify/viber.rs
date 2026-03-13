use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Viber { token: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Viber {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Viber", service_url: Some("https://www.viber.com"), setup_url: None, protocols: vec!["viber"], description: "Send messages via Viber Bot API.", attachment_support: false } }
}
#[async_trait]
impl Notify for Viber {
    fn schemas(&self) -> &[&str] { &["viber"] }
    fn service_name(&self) -> &str { "Viber" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "auth_token": self.token, "receiver": target, "type": "text", "text": msg, "sender": { "name": "Apprise" } });
            let resp = client.post("https://chatapi.viber.com/pa/send_message").header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
