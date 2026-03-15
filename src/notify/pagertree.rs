use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PagerTree { integration_id: String, verify_certificate: bool, tags: Vec<String> }
impl PagerTree {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let integration_id = url.get("id")
            .or_else(|| url.get("integration"))
            .map(|s| s.to_string())
            .or_else(|| url.host.clone().filter(|h| !h.is_empty() && h != "_"))?;
        if integration_id.is_empty() { return None; }
        // Reject if all non-alphanumeric (e.g., all plus signs decoded to spaces)
        let decoded = urlencoding::decode(&integration_id).unwrap_or_default();
        if decoded.trim().is_empty() { return None; }
        if !decoded.chars().any(|c| c.is_ascii_alphanumeric()) { return None; }
        Some(Self { integration_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "PagerTree", service_url: Some("https://pagertree.com"), setup_url: None, protocols: vec!["pagertree"], description: "Send alerts via PagerTree.", attachment_support: false } }
}
#[async_trait]
impl Notify for PagerTree {
    fn schemas(&self) -> &[&str] { &["pagertree"] }
    fn service_name(&self) -> &str { "PagerTree" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let payload = json!({ "event_type": "create", "title": ctx.title, "description": ctx.body });
        let url = format!("https://api.pagertree.com/integration/{}", self.integration_id);
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pagertree://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
