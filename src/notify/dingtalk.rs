use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct DingTalk { token: String, verify_certificate: bool, tags: Vec<String> }
impl DingTalk {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Token from ?token= or host
        let token = url.get("token").map(|s| s.to_string())
            .or_else(|| url.host.clone())?;
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) { return None; }
        // Validate secret if provided (must be alphanumeric)
        if let Some(secret) = url.get("secret") {
            if !secret.is_empty() && !secret.chars().all(|c| c.is_ascii_alphanumeric()) {
                return None;
            }
        }
        // Also check user-field as secret (dingtalk://secret@token/...)
        if let Some(ref user_secret) = url.user {
            if !user_secret.is_empty() && !user_secret.chars().all(|c| c.is_ascii_alphanumeric()) {
                return None;
            }
        }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "DingTalk", service_url: Some("https://dingtalk.com"), setup_url: None, protocols: vec!["dingtalk"], description: "Send via DingTalk robot webhook.", attachment_support: false } }
}
#[async_trait]
impl Notify for DingTalk {
    fn schemas(&self) -> &[&str] { &["dingtalk"] }
    fn service_name(&self) -> &str { "DingTalk" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://oapi.dingtalk.com/robot/send?access_token={}", self.token);
        let content = if ctx.title.is_empty() { ctx.body.clone() } else { format!("## {}\n{}", ctx.title, ctx.body) };
        let payload = json!({ "msgtype": "markdown", "markdown": { "title": ctx.title, "text": content } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "dingtalk://12345678",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "dingtalk://",
            "dingtalk://a_bd_/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
