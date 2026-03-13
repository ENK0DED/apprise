use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Synology { host: String, port: u16, user: String, password: String, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Synology {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 5001 } else { 5000 });
        Some(Self { host, port, user, password, secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Synology Chat", service_url: Some("https://www.synology.com"), setup_url: None, protocols: vec!["synology", "synologys"], description: "Send via Synology Chat.", attachment_support: false } }
}
#[async_trait]
impl Notify for Synology {
    fn schemas(&self) -> &[&str] { &["synology", "synologys"] }
    fn service_name(&self) -> &str { "Synology Chat" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let payload = json!({ "version": "1", "method": "login", "account": self.user, "passwd": self.password, "session": "apprise", "format": "cookie" });
        let login_url = format!("{}://{}:{}/webapi/auth.cgi", schema, self.host, self.port);
        let login_resp = client.post(&login_url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if !login_resp.status().is_success() { return Ok(false); }
        let notify_url = format!("{}://{}:{}/webapi/entry.cgi", schema, self.host, self.port);
        let notify_payload = json!({ "api": "SYNO.Chat.External", "method": "incoming", "version": "2", "token": "", "payload": json!({ "text": text }) });
        let resp = client.post(&notify_url).header("User-Agent", APP_ID).json(&notify_payload).send().await?;
        Ok(resp.status().is_success())
    }
}
