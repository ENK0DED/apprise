use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct BulkVs { user: String, password: String, from_phone: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl BulkVs {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let from_phone = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { user, password, from_phone, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "BulkVS", service_url: Some("https://bulkvs.com"), setup_url: None, protocols: vec!["bulkvs"], description: "Send SMS via BulkVS.", attachment_support: false } }
}
#[async_trait]
impl Notify for BulkVs {
    fn schemas(&self) -> &[&str] { &["bulkvs"] }
    fn service_name(&self) -> &str { "BulkVS" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for target in &self.targets {
            let payload = json!({ "from": self.from_phone, "to": [target], "body": msg });
            let resp = client.post("https://portal.bulkvs.com/api/v1.0/messageSend").header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.password)).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
