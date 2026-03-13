use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Mastodon {
    host: String,
    port: Option<u16>,
    token: String,
    secure: bool,
    visibility: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Mastodon {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // mastodon://token@host  or  mastodons://token@host
        let host = url.host.clone()?;
        let token = url.user.clone()?;
        let visibility = url.get("visibility").unwrap_or("public").to_string();
        Some(Self { host, port: url.port, token, secure: !url.schema.ends_with("mastodon") || url.schema.ends_with('s'), visibility, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Mastodon", service_url: Some("https://mastodon.social"), setup_url: None, protocols: vec!["mastodon", "toot", "mastodons", "toots"], description: "Post a toot on Mastodon.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Mastodon {
    fn schemas(&self) -> &[&str] { &["mastodon", "toot", "mastodons", "toots"] }
    fn service_name(&self) -> &str { "Mastodon" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/api/v1/statuses", schema, self.host, port_str);
        let status = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n\n{}", ctx.title, ctx.body) };
        let params = [("status", status.as_str()), ("visibility", self.visibility.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.token)).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
