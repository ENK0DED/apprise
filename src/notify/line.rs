use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Line { token: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Line {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "LINE", service_url: Some("https://line.me"), setup_url: None, protocols: vec!["line"], description: "Send LINE messages via bot.", attachment_support: false } }
}
#[async_trait]
impl Notify for Line {
    fn schemas(&self) -> &[&str] { &["line"] }
    fn service_name(&self) -> &str { "LINE" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "to": target, "messages": [{ "type": "text", "text": text }] });
            let resp = client.post("https://api.line.me/v2/bot/message/push").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.token)).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
