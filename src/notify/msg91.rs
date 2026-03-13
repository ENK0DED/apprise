use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Msg91 { auth_key: String, sender: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Msg91 {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let auth_key = url.password.clone()?;
        let sender = url.host.clone().unwrap_or_else(|| "APPRIS".to_string());
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { auth_key, sender, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "MSG91", service_url: Some("https://msg91.com"), setup_url: None, protocols: vec!["msg91"], description: "Send SMS via MSG91.", attachment_support: false } }
}
#[async_trait]
impl Notify for Msg91 {
    fn schemas(&self) -> &[&str] { &["msg91"] }
    fn service_name(&self) -> &str { "MSG91" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let recipients: Vec<_> = self.targets.iter().map(|t| json!({ "mobiles": t })).collect();
        let payload = json!({ "sender": self.sender, "route": "4", "country": "91", "sms": [{ "message": msg, "to": recipients }] });
        let resp = client.post("https://api.msg91.com/api/v2/sendsms").header("User-Agent", APP_ID).header("authkey", &self.auth_key).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
