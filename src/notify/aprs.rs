use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Aprs { user: String, password: String, targets: Vec<String>, tags: Vec<String> }
impl Aprs {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { user, password, targets, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "APRS", service_url: Some("https://www.aprs.org"), setup_url: None, protocols: vec!["aprs"], description: "Send messages via APRS (Amateur Radio).", attachment_support: false } }
}
#[async_trait]
impl Notify for Aprs {
    fn schemas(&self) -> &[&str] { &["aprs"] }
    fn service_name(&self) -> &str { "APRS" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let mut stream = TcpStream::connect("rotate.aprs2.net:10152").await.map_err(|e| NotifyError::Other(e.to_string()))?;
        let login = format!("user {} pass {} vers Apprise 1.9.8\r\n", self.user, self.password);
        stream.write_all(login.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
        for target in &self.targets {
            let aprs_msg = format!("{}>{}>APRS::{}:{}\r\n", self.user, self.user, target, &msg[..msg.len().min(67)]);
            stream.write_all(aprs_msg.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
        }
        Ok(true)
    }
}
