use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct WhatsApp { token: String, phone_id: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl WhatsApp {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.password.clone()?;
        let phone_id = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { token, phone_id, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "WhatsApp", service_url: Some("https://www.whatsapp.com"), setup_url: None, protocols: vec!["whatsapp"], description: "Send messages via WhatsApp Cloud API.", attachment_support: false } }
}
#[async_trait]
impl Notify for WhatsApp {
    fn schemas(&self) -> &[&str] { &["whatsapp"] }
    fn service_name(&self) -> &str { "WhatsApp" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "messaging_product": "whatsapp", "to": target, "type": "text", "text": { "body": msg } });
            let url = format!("https://graph.facebook.com/v17.0/{}/messages", self.phone_id);
            let resp = client.post(&url).header("User-Agent", APP_ID).bearer_auth(&self.token).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
