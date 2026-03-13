use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct GoogleChat {
    workspace: String,
    webhook_key: String,
    webhook_token: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl GoogleChat {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // gchat://workspace/webhook_key/webhook_token
        let workspace = url.host.clone()?;
        let webhook_key = url.path_parts.get(0)?.clone();
        let webhook_token = url.path_parts.get(1)?.clone();
        Some(Self { workspace, webhook_key, webhook_token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Google Chat", service_url: Some("https://chat.google.com"), setup_url: None, protocols: vec!["gchat"], description: "Send via Google Chat webhooks.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for GoogleChat {
    fn schemas(&self) -> &[&str] { &["gchat"] }
    fn service_name(&self) -> &str { "Google Chat" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://chat.googleapis.com/v1/spaces/{}/messages?key={}&token={}", self.workspace, self.webhook_key, self.webhook_token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("*{}*\n{}", ctx.title, ctx.body) };
        let payload = json!({ "text": text });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
