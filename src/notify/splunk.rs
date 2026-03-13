use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Splunk { token: String, host: String, port: u16, verify_certificate: bool, tags: Vec<String> }
impl Splunk {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.password.clone().or_else(|| url.user.clone())?;
        let host = url.host.clone().unwrap_or_else(|| "localhost".to_string());
        let port = url.port.unwrap_or(8088);
        Some(Self { token, host, port, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Splunk", service_url: Some("https://www.splunk.com"), setup_url: None, protocols: vec!["splunk"], description: "Send events to Splunk HEC.", attachment_support: false } }
}
#[async_trait]
impl Notify for Splunk {
    fn schemas(&self) -> &[&str] { &["splunk"] }
    fn service_name(&self) -> &str { "Splunk" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "event": { "title": ctx.title, "message": ctx.body, "severity": ctx.notify_type.to_string() } });
        let url = format!("https://{}:{}/services/collector/event", self.host, self.port);
        let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Splunk {}", self.token)).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
