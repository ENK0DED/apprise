use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Emby { host: String, port: u16, user: Option<String>, api_key: Option<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Emby {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 8920 } else { 8096 });
        Some(Self { host, port, user: url.user.clone(), api_key: url.password.clone(), secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Emby", service_url: Some("https://emby.media"), setup_url: None, protocols: vec!["emby", "embys"], description: "Send notifications to Emby.", attachment_support: false } }
}
#[async_trait]
impl Notify for Emby {
    fn schemas(&self) -> &[&str] { &["emby", "embys"] }
    fn service_name(&self) -> &str { "Emby" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let payload = json!({ "Name": "Apprise", "Description": msg, "ImageUrl": null });
        let mut url = format!("{}://{}:{}/emby/Notifications/Admin", schema, self.host, self.port);
        if let Some(key) = &self.api_key { url.push_str(&format!("?api_key={}", key)); }
        let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
        Ok(resp.status().is_success())
    }
}
