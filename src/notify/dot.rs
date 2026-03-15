use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Dot { token: String, verify_certificate: bool, tags: Vec<String> }
impl Dot {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Dot", service_url: Some("https://dot.eu.org"), setup_url: None, protocols: vec!["dot"], description: "Send via Dot notification service.", attachment_support: true } }
}
#[async_trait]
impl Notify for Dot {
    fn schemas(&self) -> &[&str] { &["dot"] }
    fn service_name(&self) -> &str { "Dot" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut payload = json!({ "token": self.token, "title": ctx.title, "body": ctx.body });
        // Use the first image attachment as a base64-encoded image field
        if let Some(att) = ctx.attachments.iter().find(|a| a.mime_type.starts_with("image/")) {
            payload["image"] = json!(base64::engine::general_purpose::STANDARD.encode(&att.data));
        }
        let resp = client.post("https://dot.eu.org/push").header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "dot://@device_id",
            "dot://apikey@device_id/text/",
            "dot://apikey@device_id/text/?refresh=no",
            "dot://apikey@device_id/text/?signature=test_signature",
            "dot://apikey@device_id/text/?link=https://example.com",
            "dot://apikey@device_id/image/?link=https://example.com&border=1&dither_type=ORDERED&dither_kernel=ATKINSON",
            "dot://apikey@device_id/image/?image=ZmFrZUJhc2U2NA==&link=https://example.com&border=1&dither_type=DIFFUSION&dither_kernel=FLOYD_STEINBERG",
            "dot://apikey@device_id/text/",
            "dot://apikey@device_id/unknown/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "dot://",
            "dot://@",
            "dot://apikey@",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
