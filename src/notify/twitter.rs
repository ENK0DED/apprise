use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Twitter { consumer_key: String, consumer_secret: String, access_token: String, access_token_secret: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl Twitter {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let consumer_key = url.user.clone()?;
        let consumer_secret = url.password.clone()?;
        let access_token = url.path_parts.get(0)?.clone();
        let access_token_secret = url.path_parts.get(1)?.clone();
        let targets: Vec<String> = url.path_parts.iter().skip(2).cloned().collect();
        Some(Self { consumer_key, consumer_secret, access_token, access_token_secret, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Twitter/X", service_url: Some("https://twitter.com"), setup_url: None, protocols: vec!["twitter", "x", "tweet"], description: "Send tweets or DMs via Twitter/X API.", attachment_support: false } }
}
#[async_trait]
impl Notify for Twitter {
    fn schemas(&self) -> &[&str] { &["twitter", "x", "tweet"] }
    fn service_name(&self) -> &str { "Twitter/X" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        // Twitter requires OAuth1.0a signing - simplified placeholder
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({ "text": &msg[..msg.len().min(280)] });
        let resp = client.post("https://api.twitter.com/2/tweets").header("User-Agent", APP_ID).bearer_auth(&self.access_token).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
