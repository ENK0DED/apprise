use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct D7Networks { token: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl D7Networks {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let token = url.host.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "D7 Networks", service_url: Some("https://d7networks.com"), setup_url: None, protocols: vec!["d7sms"], description: "Send SMS via D7 Networks.", attachment_support: false } }
}
#[async_trait]
impl Notify for D7Networks {
    fn schemas(&self) -> &[&str] { &["d7sms"] }
    fn service_name(&self) -> &str { "D7 Networks" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({
            "message_globals": { "channel": "sms" },
            "messages": [{ "recipients": self.targets, "content": msg, "data_coding": "auto" }]
        });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.d7networks.com/messages/v1/send").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.token)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 200 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "d7sms://",
            "d7sms://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}
