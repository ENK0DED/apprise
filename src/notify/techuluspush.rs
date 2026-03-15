use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct TechulusPush { token: String, verify_certificate: bool, tags: Vec<String> }
impl TechulusPush {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "TechulusPush", service_url: Some("https://push.techulus.com"), setup_url: None, protocols: vec!["push", "techuluspush"], description: "Send push via Techulus.", attachment_support: false } }
}
#[async_trait]
impl Notify for TechulusPush {
    fn schemas(&self) -> &[&str] { &["techulus", "push", "techuluspush"] }
    fn service_name(&self) -> &str { "TechulusPush" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({ "title": ctx.title, "body": ctx.body });
        let resp = client.post("https://push.techulus.com/api/v1/notify").header("User-Agent", APP_ID).header("x-api-key", &self.token).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "push://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
