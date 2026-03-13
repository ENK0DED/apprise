use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Syslog { host: String, port: u16, tags: Vec<String> }
impl Syslog {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone().unwrap_or_else(|| "localhost".to_string());
        let port = url.port.unwrap_or(514);
        Some(Self { host, port, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Syslog", service_url: None, setup_url: None, protocols: vec!["syslog"], description: "Send to local syslog.", attachment_support: false } }
}
#[async_trait]
impl Notify for Syslog {
    fn schemas(&self) -> &[&str] { &["syslog"] }
    fn service_name(&self) -> &str { "Syslog" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        use std::net::UdpSocket;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let pri = 14u8; // user.info
        let syslog_msg = format!("<{}>{}", pri, msg);
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NotifyError::Other(e.to_string()))?;
        socket.send_to(syslog_msg.as_bytes(), format!("{}:{}", self.host, self.port)).map_err(|e| NotifyError::Other(e.to_string()))?;
        Ok(true)
    }
}
