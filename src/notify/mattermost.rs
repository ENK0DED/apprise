use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Mattermost {
    host: String,
    port: Option<u16>,
    token: String,
    channels: Vec<String>,
    secure: bool,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Mattermost {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // mmost://host/token  or  mmosts://host/token[/channel...]
        let host = url.host.clone()?;
        let token = url.path_parts.first()?.clone();
        let channels = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
        Some(Self { host, port: url.port, token, channels, secure: url.schema == "mmosts", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Mattermost", service_url: Some("https://mattermost.com"), setup_url: None, protocols: vec!["mmost", "mmosts"], description: "Send via Mattermost webhooks.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Mattermost {
    fn schemas(&self) -> &[&str] { &["mmost", "mmosts"] }
    fn service_name(&self) -> &str { "Mattermost" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/hooks/{}", schema, self.host, port_str, self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let mut payload = json!({ "text": text });
        if !self.channels.is_empty() { payload["channel"] = json!(format!("#{}", self.channels[0])); }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
