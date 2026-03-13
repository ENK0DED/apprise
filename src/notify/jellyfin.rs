use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Jellyfin { host: String, port: Option<u16>, apikey: String, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Jellyfin {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let apikey = url.user.clone()?;
        Some(Self { host, port: url.port, apikey, secure: url.schema == "jellyfinc", verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Jellyfin", service_url: Some("https://jellyfin.org"), setup_url: None, protocols: vec!["jellyfin", "jellyfinc"], description: "Send notifications via Jellyfin.", attachment_support: false } }
}
#[async_trait]
impl Notify for Jellyfin {
    fn schemas(&self) -> &[&str] { &["jellyfin", "jellyfinc"] }
    fn service_name(&self) -> &str { "Jellyfin" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/Notifications/Admin", schema, self.host, port_str);
        let payload = json!({ "Name": ctx.title, "Description": ctx.body, "ImageUrl": serde_json::Value::Null, "Url": serde_json::Value::Null });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).header("X-Emby-Authorization", format!("MediaBrowser Token=\"{}\"", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}
