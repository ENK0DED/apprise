use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Growl { host: String, port: u16, password: Option<String>, tags: Vec<String> }
impl Growl {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let port = url.port.unwrap_or(23053);
        Some(Self { host, port, password: url.password.clone(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Growl", service_url: Some("http://growl.info"), setup_url: None, protocols: vec!["growl"], description: "Send notifications via Growl (GNTP).", attachment_support: false } }
}
#[async_trait]
impl Notify for Growl {
    fn schemas(&self) -> &[&str] { &["growl"] }
    fn service_name(&self) -> &str { "Growl" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;
        // GNTP protocol (simplified, no full MD5 auth)
        let mut stream = TcpStream::connect(format!("{}:{}", self.host, self.port)).await.map_err(|e| NotifyError::Other(e.to_string()))?;
        let register = format!("GNTP/1.0 REGISTER NONE\r\nApplication-Name: Apprise\r\nNotification-Count: 1\r\n\r\nNotification-Name: Alert\r\n\r\n");
        stream.write_all(register.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
        let notify = format!("GNTP/1.0 NOTIFY NONE\r\nApplication-Name: Apprise\r\nNotification-Name: Alert\r\nNotification-Title: {}\r\nNotification-Text: {}\r\n\r\n", ctx.title, ctx.body);
        stream.write_all(notify.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
        Ok(true)
    }
}
