use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Kumulos { apikey: String, server_key: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Kumulos {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.user.clone()?;
        let server_key = url.password.clone()?;
        let targets = url.path_parts.clone();
        Some(Self { apikey, server_key, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Kumulos", service_url: Some("https://kumulos.com"), setup_url: None, protocols: vec!["kumulos"], description: "Send push notifications via Kumulos.", attachment_support: false } }
}
#[async_trait]
impl Notify for Kumulos {
    fn schemas(&self) -> &[&str] { &["kumulos"] }
    fn service_name(&self) -> &str { "Kumulos" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let payload = json!({ "target": { "broadcast": true }, "content": { "title": ctx.title, "message": ctx.body } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://messages.kumulos.com/v2/notifications").header("User-Agent", APP_ID).basic_auth(&self.apikey, Some(&self.server_key)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "kumulos://",
            "kumulos://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
