use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PushPlus { token: String, verify_certificate: bool, tags: Vec<String> }
impl PushPlus {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.get("token").map(|s| s.to_string())
            .or_else(|| url.host.clone().filter(|h| !h.is_empty() && h != "www.pushplus.plus"))?;
        // Reject tokens with invalid chars
        if token.contains('!') || token.contains('%') || token.trim().is_empty() { return None; }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "PushPlus", service_url: Some("https://www.pushplus.plus"), setup_url: None, protocols: vec!["pushplus"], description: "Send notifications via PushPlus.", attachment_support: false } }
}
#[async_trait]
impl Notify for PushPlus {
    fn schemas(&self) -> &[&str] { &["pushplus"] }
    fn service_name(&self) -> &str { "PushPlus" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let payload = json!({ "token": self.token, "title": ctx.title, "content": ctx.body });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://www.pushplus.plus/send").header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "pushplus://abc123def456ghi789jkl012mno345pq",
            "pushplus://?token=abc123def456ghi789jkl012mno345pq",
            "https://www.pushplus.plus/send?token=abc123def456ghi789jkl012mno345pq",
            "pushplus://ffffffffffffffffffffffffffffffff",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pushplus://",
            "pushplus://invalid!",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
