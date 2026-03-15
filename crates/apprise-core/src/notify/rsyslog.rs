use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct RSyslog { host: String, port: u16, tags: Vec<String> }
impl RSyslog {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone().unwrap_or_else(|| "localhost".to_string());
        let port = url.port.unwrap_or(514);
        Some(Self { host, port, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "RSyslog", service_url: None, setup_url: None, protocols: vec!["rsyslog"], description: "Send via RSyslog (UDP).", attachment_support: false } }
}
#[async_trait]
impl Notify for RSyslog {
    fn schemas(&self) -> &[&str] { &["rsyslog"] }
    fn service_name(&self) -> &str { "RSyslog" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        use std::net::UdpSocket;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let pri = 14u8;
        let syslog_msg = format!("<{}>{}", pri, msg);
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NotifyError::Other(e.to_string()))?;
        socket.send_to(syslog_msg.as_bytes(), format!("{}:{}", self.host, self.port)).map_err(|e| NotifyError::Other(e.to_string()))?;
        Ok(true)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::parse::ParsedUrl;

    #[test]
    fn test_valid_urls() {
        let valid_urls = vec![
            "rsyslog://localhost",
            "rsyslog://localhost:514",
            "rsyslog://192.168.1.1",
            "rsyslog://syslog.example.com:1514",
        ];
        for url in &valid_urls {
            let parsed = ParsedUrl::parse(url);
            assert!(parsed.is_some(), "ParsedUrl::parse failed for: {}", url);
            let parsed = parsed.unwrap();
            assert!(
                RSyslog::from_url(&parsed).is_some(),
                "RSyslog::from_url returned None for valid URL: {}",
                url,
            );
        }
    }

    #[test]
    fn test_rsyslog_default_port() {
        let parsed = ParsedUrl::parse("rsyslog://localhost").unwrap();
        let rs = RSyslog::from_url(&parsed).unwrap();
        assert_eq!(rs.host, "localhost");
        assert_eq!(rs.port, 514);
    }

    #[test]
    fn test_rsyslog_custom_port() {
        let parsed = ParsedUrl::parse("rsyslog://localhost:518").unwrap();
        let rs = RSyslog::from_url(&parsed).unwrap();
        assert_eq!(rs.port, 518);
    }

    #[test]
    fn test_rsyslog_host_parsing() {
        let parsed = ParsedUrl::parse("rsyslog://syslog.example.com").unwrap();
        let rs = RSyslog::from_url(&parsed).unwrap();
        assert_eq!(rs.host, "syslog.example.com");
    }

    #[test]
    fn test_rsyslog_static_details() {
        let details = RSyslog::static_details();
        assert_eq!(details.service_name, "RSyslog");
        assert_eq!(details.protocols, vec!["rsyslog"]);
        assert!(!details.attachment_support);
    }
}
