use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Smpp { host: String, port: u16, user: String, password: String, targets: Vec<String>, from: String, tags: Vec<String> }
impl Smpp {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let port = url.port.unwrap_or(2775);
        let from = url.get("from").unwrap_or("Apprise").to_string();
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { host, port, user, password, targets, from, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SMPP", service_url: None, setup_url: None, protocols: vec!["smpp"], description: "Send SMS via SMPP protocol.", attachment_support: false } }
}
#[async_trait]
impl Notify for Smpp {
    fn schemas(&self) -> &[&str] { &["smpp"] }
    fn service_name(&self) -> &str { "SMPP" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        // SMPP requires binary protocol implementation - simplified stub
        // A real implementation would use a crate like rust-smpp
        let _msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        Err(NotifyError::Other(format!("SMPP not fully implemented; would connect to {}:{} as {}", self.host, self.port, self.user)))
    }
}
