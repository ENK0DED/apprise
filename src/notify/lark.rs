use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Lark { token: String, verify_certificate: bool, tags: Vec<String> }
impl Lark {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = if url.schema == "https" || url.schema == "http" {
            // HTTPS URL: token is the last path part
            url.path_parts.last().cloned()
        } else {
            url.host.clone().filter(|h| !h.is_empty() && h != "_")
                .or_else(|| url.get("token").map(|s| s.to_string()))
        }?;
        if token.is_empty() { return None; }
        Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Lark", service_url: Some("https://larksuite.com"), setup_url: None, protocols: vec!["lark"], description: "Send via Lark (Feishu international) webhooks.", attachment_support: false } }
}
#[async_trait]
impl Notify for Lark {
    fn schemas(&self) -> &[&str] { &["lark"] }
    fn service_name(&self) -> &str { "Lark" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://open.larksuite.com/open-apis/bot/v2/hook/{}", self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
        let payload = json!({ "msg_type": "text", "content": { "text": text } });
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
            "lark://",
            "lark://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
