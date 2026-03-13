use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Xbmc { host: String, port: u16, user: Option<String>, password: Option<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Xbmc {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 443 } else { 8080 });
        Some(Self { host, port, user: url.user.clone(), password: url.password.clone(), secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Kodi/XBMC", service_url: Some("https://kodi.tv"), setup_url: None, protocols: vec!["xbmc", "kodi", "kodis"], description: "Send notifications to Kodi/XBMC.", attachment_support: false } }
}
#[async_trait]
impl Notify for Xbmc {
    fn schemas(&self) -> &[&str] { &["xbmc", "kodi", "kodis"] }
    fn service_name(&self) -> &str { "Kodi/XBMC" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let payload = json!({ "jsonrpc": "2.0", "method": "GUI.ShowNotification", "params": { "title": ctx.title, "message": ctx.body, "displaytime": 5000 }, "id": 1 });
        let url = format!("{}://{}:{}/jsonrpc", schema, self.host, self.port);
        let mut req = client.post(&url).header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req.send().await?;
        Ok(resp.status().is_success())
    }
}
