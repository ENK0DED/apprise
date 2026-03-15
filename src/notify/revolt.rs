use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Revolt { bot_token: String, channel_id: String, verify_certificate: bool, tags: Vec<String> }
impl Revolt {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let bot_token = url.get("bot_token").map(|s| s.to_string())
            .or_else(|| url.host.clone().filter(|h| !h.is_empty() && h != "_"))?;
        if bot_token.is_empty() { return None; }
        // Channel from path, ?channel=, or ?to=
        let channel_id = url.path_parts.first().cloned()
            .or_else(|| url.get("channel").map(|s| s.split(',').next().unwrap_or("").trim().to_string()))
            .or_else(|| url.get("to").map(|s| s.split(',').next().unwrap_or("").trim().to_string()))?;
        if channel_id.is_empty() { return None; }
        Some(Self { bot_token, channel_id, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Revolt", service_url: Some("https://revolt.chat"), setup_url: None, protocols: vec!["revolt"], description: "Send messages via Revolt.", attachment_support: false } }
}
#[async_trait]
impl Notify for Revolt {
    fn schemas(&self) -> &[&str] { &["revolt"] }
    fn service_name(&self) -> &str { "Revolt" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.revolt.chat/channels/{}/messages", self.channel_id);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "content": text });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("X-Bot-Token", self.bot_token.as_str()).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "revolt://",
            "revolt://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
