use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Bark { host: String, port: Option<u16>, device_keys: Vec<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Bark {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let device_keys = url.path_parts.clone();
        if device_keys.is_empty() { return None; }
        Some(Self { host, port: url.port, device_keys, secure: url.schema == "barks", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Bark", service_url: Some("https://bark.day.app"), setup_url: None, protocols: vec!["bark", "barks"], description: "Send notifications to iOS devices via Bark.", attachment_support: false } }
}
#[async_trait]
impl Notify for Bark {
    fn schemas(&self) -> &[&str] { &["bark", "barks"] }
    fn service_name(&self) -> &str { "Bark" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;
        for key in &self.device_keys {
            let url = format!("{}://{}{}push", schema, self.host, port_str);
            let payload = json!({ "device_key": key, "title": ctx.title, "body": ctx.body });
            let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}
