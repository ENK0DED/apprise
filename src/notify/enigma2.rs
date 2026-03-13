use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Enigma2 { host: String, port: u16, user: Option<String>, password: Option<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Enigma2 {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 443 } else { 80 });
        Some(Self { host, port, user: url.user.clone(), password: url.password.clone(), secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Enigma2", service_url: None, setup_url: None, protocols: vec!["enigma2", "enigma2s"], description: "Send notifications to Enigma2 receivers.", attachment_support: false } }
}
#[async_trait]
impl Notify for Enigma2 {
    fn schemas(&self) -> &[&str] { &["enigma2", "enigma2s"] }
    fn service_name(&self) -> &str { "Enigma2" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let url = format!("{}://{}:{}/web/message?text={}&type=1", schema, self.host, self.port, urlencoding::encode(&msg));
        let mut req = client.get(&url).header("User-Agent", APP_ID);
        if let (Some(u), Some(p)) = (&self.user, &self.password) {
            req = req.basic_auth(u, Some(p));
        }
        let resp = req.send().await?;
        Ok(resp.status().is_success())
    }
}
